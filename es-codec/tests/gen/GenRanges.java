import java.nio.file.*;
import org.apache.lucene.analysis.standard.StandardAnalyzer;
import org.apache.lucene.document.*;
import org.apache.lucene.index.*;
import org.apache.lucene.store.*;

/**
 * Writes the index read by tests/points.rs: two-dimensional LongRange trees, as Elasticsearch's
 * long_range and date_range fields index them, flushed once (no merge). The value formulas must
 * match the test's.
 */
public class GenRanges {
  static final int DOCS = 3000;

  public static void main(String[] args) throws Exception {
    try (Directory dir = FSDirectory.open(Paths.get(args[0]));
        IndexWriter w = new IndexWriter(dir, new IndexWriterConfig(new StandardAnalyzer()).setUseCompoundFile(true))) {
      for (int d = 0; d < DOCS; d++) {
        Document doc = new Document();
        // distinct ranges, negative to positive, of growing width: high-cardinality leaves
        long from = (d - 1500) * 1_000_003L;
        doc.add(new LongRange("wide", new long[] {from}, new long[] {from + d * 7L}));
        // few distinct ranges, two per doc on every third doc: low-cardinality leaves
        if (d % 3 == 0) {
          doc.add(new LongRange("few", new long[] {d % 5}, new long[] {d % 5 + 10}));
          doc.add(new LongRange("few", new long[] {-1}, new long[] {d % 7}));
        }
        // one leaf whose values are all equal
        if (d % 100 == 0) doc.add(new LongRange("same", new long[] {3}, new long[] {9}));
        w.addDocument(doc);
      }
    }
  }
}
