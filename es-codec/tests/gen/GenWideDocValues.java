import java.nio.file.*;
import org.apache.lucene.analysis.standard.StandardAnalyzer;
import org.apache.lucene.document.*;
import org.apache.lucene.index.*;
import org.apache.lucene.store.*;
import org.apache.lucene.util.BytesRef;

/**
 * Writes the index read by tests/wide_doc_values.rs (review finding D12): multi-valued and sparse
 * sorted set and sorted numeric fields, with more than 1024 distinct terms so the terms
 * dictionary has several reverse-index blocks. Values must match `kw` and `num` there.
 */
public class GenWideDocValues {
  static final int DOCS = 3000;

  static String[] kw(int d) { // every third doc has none; up to three of 2000 terms
    if (d % 3 == 0) return null;
    int n = 1 + d % 3;
    String[] out = new String[n];
    for (int i = 0; i < n; i++) out[i] = String.format("term-%04d", (d * 7 + i * 613) % 2000);
    return out;
  }

  static long[] num(int d) { // every fifth doc has none; up to two values, negative and large
    if (d % 5 == 0) return null;
    return d % 2 == 0 ? new long[] {d * 1_000_003L - 1_500_000_000L} : new long[] {-d, (long) d << 33};
  }

  public static void main(String[] args) throws Exception {
    try (Directory dir = FSDirectory.open(Paths.get(args[0]));
        IndexWriter w = new IndexWriter(dir, new IndexWriterConfig(new StandardAnalyzer()).setUseCompoundFile(true))) {
      for (int d = 0; d < DOCS; d++) {
        Document doc = new Document();
        String[] ks = kw(d);
        if (ks != null) for (String k : ks) doc.add(new SortedSetDocValuesField("kw", new BytesRef(k)));
        long[] ns = num(d);
        if (ns != null) for (long n : ns) doc.add(new SortedNumericDocValuesField("num", n));
        w.addDocument(doc);
      }
      w.forceMerge(1);
    }
  }
}
