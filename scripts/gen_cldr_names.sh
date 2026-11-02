#!/usr/bin/env bash
# Regenerates es-index/src/date_pattern/cldr_names.tsv: the month, day-of-week, AM/PM and era
# names of every `lang` and `lang_COUNTRY` locale in the CLDR data of Elasticsearch 9.5.5's JDK
# (run in its Docker image, capped at 1 CPU / 1 GB, removed afterwards).
# Row: tag, then 18 `|`-joined fields: months and days each in SHORT, FULL, NARROW and their
# STANDALONE forms, then AM/PM and eras in SHORT, FULL, NARROW. An empty field takes the
# language's row's field for a `lang_COUNTRY` row, and the format form for a standalone one.
set -euo pipefail
cd "$(dirname "$0")/.."
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cat > "$work/Names.java" <<'EOF'
import java.time.*; import java.time.format.*; import java.time.temporal.*; import java.util.*;
public class Names {
  static final TextStyle[] STYLES = {TextStyle.SHORT, TextStyle.FULL, TextStyle.NARROW,
      TextStyle.SHORT_STANDALONE, TextStyle.FULL_STANDALONE, TextStyle.NARROW_STANDALONE};
  static String names(Locale l, ChronoField f, TextStyle s, int from, int to) {
    DateTimeFormatter fmt = new DateTimeFormatterBuilder().appendText(f, s).toFormatter(l);
    StringJoiner j = new StringJoiner("|");
    for (int v = from; v <= to; v++) {
      j.add(fmt.format(switch (f) {
        case MONTH_OF_YEAR -> LocalDateTime.of(2021, v, 1, 0, 0);
        case DAY_OF_WEEK -> LocalDateTime.of(2021, 5, 2 + v, 0, 0);
        case AMPM_OF_DAY -> LocalDateTime.of(2021, 5, 1, v * 12, 0);
        default -> LocalDateTime.of(v == 0 ? -5 : 2021, 5, 1, 0, 0);
      }));
    }
    return j.toString();
  }
  static String[] row(Locale l) {
    List<String> out = new ArrayList<>();
    for (TextStyle s : STYLES) out.add(names(l, ChronoField.MONTH_OF_YEAR, s, 1, 12));
    for (TextStyle s : STYLES) out.add(names(l, ChronoField.DAY_OF_WEEK, s, 1, 7));
    for (int i = 0; i < 3; i++) out.add(names(l, ChronoField.AMPM_OF_DAY, STYLES[i], 0, 1));
    for (int i = 0; i < 3; i++) out.add(names(l, ChronoField.ERA, STYLES[i], 0, 1));
    return out.toArray(new String[0]);
  }
  public static void main(String[] a) {
    TreeMap<String, Locale> tags = new TreeMap<>(Map.of("root", Locale.ROOT));
    for (Locale l : Locale.getAvailableLocales()) {
      if (l.getScript().isEmpty() && l.getVariant().isEmpty() && !l.getLanguage().isEmpty()) {
        String c = l.getCountry();
        tags.put(l.getLanguage() + (c.isEmpty() ? "" : "_" + c), Locale.of(l.getLanguage(), c));
      }
    }
    for (var e : tags.entrySet()) {
      String[] row = row(e.getValue()), out = row.clone();
      int sep = e.getKey().indexOf('_');
      String[] lang = sep < 0 ? null : row(tags.get(e.getKey().substring(0, sep)));
      for (int i = 0; i < out.length; i++) {
        boolean standalone = i < 12 && i % 6 >= 3;
        if (lang != null ? row[i].equals(lang[i]) : standalone && row[i].equals(row[i - 3])) out[i] = "";
      }
      if (lang == null || !Arrays.equals(row, lang)) System.out.println(e.getKey() + "\t" + String.join("\t", out));
    }
  }
}
EOF
docker run --rm --cpus 1 --memory 1g -v "$work:/w:ro" -w /w \
  --entrypoint /usr/share/elasticsearch/jdk/bin/java docker.elastic.co/elasticsearch/elasticsearch:9.5.5 \
  -Djava.locale.providers=CLDR -Dstdout.encoding=UTF-8 Names.java \
  > es-index/src/date_pattern/cldr_names.tsv
