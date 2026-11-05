import java.nio.file.*;
import org.apache.lucene.analysis.standard.StandardAnalyzer;
import org.apache.lucene.document.*;
import org.apache.lucene.index.*;
import org.apache.lucene.store.*;

/**
 * Writes the index read by tests/points.rs: LongRange trees whose docs hold several ranges equal
 * in one dimension, so Lucene's flush (MutablePointTreeReaderUtils' radix selector and intro
 * sorter) decides their order, flushed once (no merge). The value formulas must match the test's.
 */
public class GenRangeTies {
  static final int DOCS = 4000;

  public static void main(String[] args) throws Exception {
    try (Directory dir = FSDirectory.open(Paths.get(args[0]));
        IndexWriter w = new IndexWriter(dir, new IndexWriterConfig(new StandardAnalyzer()).setUseCompoundFile(true))) {
      for (int d = 0; d < DOCS; d++) {
        Document doc = new Document();
        // six ranges per doc, three sharing their start and three their end: two dimensions
        for (long k : new long[] {3, 1, 2}) {
          doc.add(new LongRange("ties", new long[] {d % 50}, new long[] {d % 50 + k}));
        }
        for (long k : new long[] {2, 0, 1}) {
          doc.add(new LongRange("ties", new long[] {d % 50 - k}, new long[] {d % 50 + 5}));
        }
        // three boxes per doc sharing their low corner: four dimensions
        for (long k : new long[] {2, 3, 1}) {
          doc.add(new LongRange("box", new long[] {d % 37, d % 11}, new long[] {d % 37 + k, d % 11 + 2 * k}));
        }
        // one leaf of four docs whose ranges tie in the dimension the leaf sorts by
        if (d % 1000 == 0) {
          for (long k : new long[] {3, 1, 2}) {
            doc.add(new LongRange("leaf", new long[] {k}, new long[] {5}));
          }
          doc.add(new LongRange("leaf", new long[] {4}, new long[] {6}));
        }
        w.addDocument(doc);
      }
    }
  }
}
