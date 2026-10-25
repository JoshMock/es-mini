//! Elasticsearch's default t-digest for the `percentiles` aggregation, ported from
//! `HybridDigest`, `SortingDigest`, `MergingDigest` and `Dist` (libs/tdigest) at 9.5.5: values
//! are kept exactly (and percentiles interpolated between them) up to 20 times the compression,
//! then summarized into centroids sized by the K_2 scale function.

/// A t-digest: every value until it holds `max_sorting`, then a merging digest.
#[derive(Debug, Clone)]
pub(crate) struct TDigest {
    pub(crate) compression: f64,
    max_sorting: u64,
    digest: Digest,
}

#[derive(Debug, Clone)]
enum Digest {
    Sorting(Vec<f64>),
    Merging(Box<Merging>),
}

impl TDigest {
    pub(crate) fn new(compression: f64) -> Self {
        TDigest {
            compression,
            // Java's Math.round(compression) * 20
            max_sorting: (compression + 0.5).floor() as u64 * 20,
            digest: Digest::Sorting(vec![]),
        }
    }

    pub(crate) fn add(&mut self, x: f64, w: u64) {
        self.reserve(w);
        match &mut self.digest {
            Digest::Sorting(v) => v.extend(std::iter::repeat_n(x, w as usize)),
            Digest::Merging(m) => m.add(x, w as f64),
        }
    }

    /// Adds another digest's centroids, as ES reduces shard results.
    pub(crate) fn add_digest(&mut self, other: &mut TDigest) {
        self.reserve(other.size());
        for (mean, count) in other.centroids() {
            match &mut self.digest {
                Digest::Sorting(v) => v.extend(std::iter::repeat_n(mean, count as usize)),
                Digest::Merging(m) => m.add(mean, count as f64),
            }
        }
    }

    /// Switches to a merging digest once `size` more values would reach `max_sorting`.
    fn reserve(&mut self, size: u64) {
        if let Digest::Sorting(v) = &self.digest
            && v.len() as u64 + size >= self.max_sorting
        {
            let mut m = Merging::new(self.compression);
            v.iter().for_each(|&x| m.add(x, 1.0));
            self.digest = Digest::Merging(Box::new(m));
        }
    }

    /// A data node's reduced digest as ES's coordinating node reduces it: its centroids are
    /// transported (read into a new digest, then compressed), then added to a new digest.
    pub(crate) fn coordinated(mut self) -> TDigest {
        let mut t = TDigest::new(self.compression);
        t.add_digest(&mut self);
        if let Digest::Merging(m) = &mut t.digest {
            m.merge_new_values(m.public_compression);
        }
        let mut r = TDigest::new(self.compression);
        r.add_digest(&mut t);
        r
    }

    pub(crate) fn size(&self) -> u64 {
        match &self.digest {
            Digest::Sorting(v) => v.len() as u64,
            Digest::Merging(m) => (m.total_weight + m.unmerged_weight) as u64,
        }
    }

    fn centroids(&mut self) -> Vec<(f64, u64)> {
        match &mut self.digest {
            Digest::Sorting(v) => {
                v.sort_by(f64::total_cmp);
                v.iter().map(|&x| (x, 1)).collect()
            }
            Digest::Merging(m) => {
                m.merge_new_values(m.compression);
                let n = m.last_used;
                (m.mean[..n].iter().zip(&m.weight[..n]))
                    .map(|(&mean, &w)| (mean, w as u64))
                    .collect()
            }
        }
    }

    /// The value at quantile `q` (in `[0, 1]`); NaN without values.
    pub(crate) fn quantile(&mut self, q: f64) -> f64 {
        match &mut self.digest {
            Digest::Sorting(v) => {
                v.sort_by(f64::total_cmp);
                sorted_quantile(q, v)
            }
            Digest::Merging(m) => m.quantile(q),
        }
    }

    /// The fraction of values at or below `x`, NaN without values: `MergingDigest.cdf`, which
    /// `Dist.cdf` matches with unit weights.
    pub(crate) fn cdf(&mut self, x: f64) -> f64 {
        let c = self.centroids();
        let (min, max) = match &self.digest {
            Digest::Merging(m) => (m.min, m.max),
            Digest::Sorting(_) => (
                c.first().map_or(f64::NAN, |c| c.0),
                c.last().map_or(f64::NAN, |c| c.0),
            ),
        };
        let n = c.len();
        let size = c.iter().map(|&(_, w)| w as f64).sum::<f64>();
        let interpolate = |x: f64, x0: f64, x1: f64| (x - x0) / (x1 - x0);
        match n {
            0 => return f64::NAN,
            1 if x < min => return 0.0,
            1 if x > max => return 1.0,
            1 => return 0.5,
            _ if x < min => return 0.0,
            _ if x == min => {
                let dw: u64 = c
                    .iter()
                    .take_while(|&&(m, _)| m == x)
                    .map(|&(_, w)| w)
                    .sum();
                return dw as f64 / 2.0 / size;
            }
            _ if x > max => return 1.0,
            _ if x == max => {
                let dw: u64 = (c.iter().rev().take_while(|&&(m, _)| m == x))
                    .map(|&(_, w)| w)
                    .sum();
                return (size - dw as f64 / 2.0) / size;
            }
            _ => {}
        }
        let mut left = (c[1].0 - c[0].0) / 2.0;
        let mut so_far = 0.0;
        for i in 0..n - 1 {
            let (mean, w) = (c[i].0, c[i].1 as f64);
            let right = (c[i + 1].0 - mean) / 2.0;
            if x < mean + right {
                let v = (so_far + w * interpolate(x, mean - left, mean + right)) / size;
                return v.max(0.0);
            }
            so_far += w;
            left = right;
        }
        let (mean, w) = (c[n - 1].0, c[n - 1].1 as f64);
        let right = (mean - c[n - 2].0) / 2.0;
        match x < mean + right {
            true => (so_far + w * interpolate(x, mean - right, mean + right)) / size,
            false => 1.0,
        }
    }
}

/// `Dist.quantile`: linear interpolation between the sorted values around `q`.
fn sorted_quantile(q: f64, v: &[f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let index = q * (v.len() - 1) as f64;
    let low = (index.floor() as usize).min(v.len() - 1);
    let high = (low + 1).min(v.len() - 1);
    match index {
        i if i <= 0.0 => v[0],
        i if i >= (v.len() - 1) as f64 => v[v.len() - 1],
        i => v[low] + (i - low as f64) * (v[high] - v[low]),
    }
}

fn weighted_average(x1: f64, w1: f64, x2: f64, w2: f64) -> f64 {
    let (x1, w1, x2, w2) = if x1 <= x2 {
        (x1, w1, x2, w2)
    } else {
        (x2, w2, x1, w1)
    };
    ((x1 * w1 + x2 * w2) / (w1 + w2)).clamp(x1, x2)
}

/// `MergingDigest` with its defaults: weight limits, alternating merge direction and two-level
/// compression.
#[derive(Debug, Clone)]
struct Merging {
    public_compression: f64,
    compression: f64,
    merge_count: u64,
    last_used: usize,
    total_weight: f64,
    weight: Vec<f64>,
    mean: Vec<f64>,
    unmerged_weight: f64,
    temp_used: usize,
    temp_weight: Vec<f64>,
    temp_mean: Vec<f64>,
    min: f64,
    max: f64,
}

impl Merging {
    fn new(compression: f64) -> Self {
        let public_compression = compression.max(10.0);
        // size fudge 10 for the weight limit; the buffer holds 5 times the centroids
        let size = (public_compression + 10.0) as usize;
        let buffer = 5 * size;
        let scale = (buffer / size - 1).max(1) as f64;
        let compression = scale.sqrt() * public_compression;
        let size = size.max((compression + 10.0).ceil() as usize);
        let buffer = buffer.max(2 * size);
        Merging {
            public_compression,
            compression,
            merge_count: 0,
            last_used: 0,
            total_weight: 0.0,
            weight: vec![0.0; size],
            mean: vec![0.0; size],
            unmerged_weight: 0.0,
            temp_used: 0,
            temp_weight: vec![0.0; buffer],
            temp_mean: vec![0.0; buffer],
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        }
    }

    fn add(&mut self, x: f64, w: f64) {
        if self.temp_used >= self.temp_weight.len() - self.last_used - 1 {
            self.merge_new_values(self.compression);
        }
        self.temp_weight[self.temp_used] = w;
        self.temp_mean[self.temp_used] = x;
        self.temp_used += 1;
        self.unmerged_weight += w;
        self.min = self.min.min(x);
        self.max = self.max.max(x);
    }

    fn merge_new_values(&mut self, compression: f64) {
        if self.unmerged_weight > 0.0 {
            // every other merge runs in reverse to avoid left-to-right bias
            self.merge(self.merge_count % 2 == 1, compression);
            self.merge_count += 1;
            self.temp_used = 0;
            self.unmerged_weight = 0.0;
        }
    }

    fn merge(&mut self, backwards: bool, compression: f64) {
        let (used, last) = (self.temp_used, self.last_used);
        self.temp_mean[used..used + last].copy_from_slice(&self.mean[..last]);
        self.temp_weight[used..used + last].copy_from_slice(&self.weight[..last]);
        let count = used + last;
        let (in_mean, in_weight) = (&self.temp_mean, &mut self.temp_weight);
        // Sort.stableSort orders by value, then by position
        let mut order: Vec<usize> = (0..count).collect();
        order.sort_by(|&a, &b| in_mean[a].total_cmp(&in_mean[b]));
        self.total_weight += self.unmerged_weight;
        if backwards {
            order.reverse();
        }
        let total = self.total_weight;
        let mut cell = 0;
        self.mean[0] = in_mean[order[0]];
        self.weight[0] = in_weight[order[0]];
        let mut w_so_far = 0.0;
        // K_2's normalizer: compression / Z(compression, n)
        let normalizer = compression / (4.0 * (total / compression).ln() + 24.0);
        let max = |q: f64| q * (1.0 - q) / normalizer;
        for (i, &ix) in order.iter().enumerate().skip(1) {
            let proposed = self.weight[cell] + in_weight[ix];
            let q0 = w_so_far / total;
            let q2 = (w_so_far + proposed) / total;
            let mut add_this = proposed <= total * max(q0).min(max(q2));
            // the first and last centroids never merge
            if i == 1 || i == count - 1 {
                add_this = false;
            }
            if cell == self.mean.len() - 1 {
                add_this = true;
            }
            if add_this {
                self.mean[cell] = weighted_average(
                    self.mean[cell],
                    self.weight[cell],
                    in_mean[ix],
                    in_weight[ix],
                );
                self.weight[cell] += in_weight[ix];
            } else {
                w_so_far += self.weight[cell];
                cell += 1;
                self.mean[cell] = in_mean[ix];
                self.weight[cell] = in_weight[ix];
            }
            in_weight[ix] = 0.0;
        }
        self.last_used = cell + 1;
        if backwards {
            self.mean[..cell + 1].reverse();
            self.weight[..cell + 1].reverse();
        }
        if total > 0.0 {
            self.min = self.min.min(self.mean[0]);
            self.max = self.max.max(self.mean[cell]);
        }
    }

    fn quantile(&mut self, q: f64) -> f64 {
        self.merge_new_values(self.compression);
        let (n, mean, weight) = (self.last_used, &self.mean, &self.weight);
        match n {
            0 => return f64::NAN,
            1 => return mean[0],
            _ => {}
        }
        let total = self.total_weight;
        let index = q * total;
        if index < 0.0 {
            return self.min;
        }
        if index >= total {
            return self.max;
        }
        let mut w_so_far = weight[0] / 2.0;
        // a single sample sits at min (and max), so interpolate towards it
        if weight[0] > 1.0 && index < w_so_far {
            return weighted_average(self.min, w_so_far - index, mean[0], index);
        }
        if weight[n - 1] > 1.0 && total - index <= weight[n - 1] / 2.0 {
            return self.max
                - (total - index - 1.0) / (weight[n - 1] / 2.0 - 1.0) * (self.max - mean[n - 1]);
        }
        for i in 0..n - 1 {
            let dw = (weight[i] + weight[i + 1]) / 2.0;
            if w_so_far + dw > index {
                let z1 = index - w_so_far;
                let z2 = w_so_far + dw - index;
                return weighted_average(mean[i], z2, mean[i + 1], z1);
            }
            w_so_far += dw;
        }
        let z1 = index - w_so_far;
        let z2 = weight[n - 1] / 2.0 - z1;
        weighted_average(mean[n - 1], z1, self.max, z2)
    }
}
