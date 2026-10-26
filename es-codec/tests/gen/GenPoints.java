import java.nio.file.*;
import org.apache.lucene.analysis.standard.StandardAnalyzer;
import org.apache.lucene.document.*;
import org.apache.lucene.index.*;
import org.apache.lucene.store.*;

/**
 * Writes the index read by tests/points.rs: multi-leaf BKD trees whose leaves use each doc id
 * encoding up to 21 bits and each value encoding. The value formulas must match the test's.
 */
public class GenPoints {
  static final int DOCS = 70_000; // a leaf of "mod" spans more than 65536 doc ids (21-bit ids)

  public static void main(String[] args) throws Exception {
    try (Directory dir = FSDirectory.open(Paths.get(args[0]));
        IndexWriter w = new IndexWriter(dir, new IndexWriterConfig(new StandardAnalyzer()).setUseCompoundFile(true))) {
      for (int d = 0; d < DOCS; d++) {
        Document doc = new Document();
        doc.add(new IntPoint("seq", d)); // contiguous doc ids, distinct values
        if (d % 4 == 0) doc.add(new IntPoint("mod", d % 1000)); // low cardinality, 21-bit ids
        if (d % 3 == 0) doc.add(new IntPoint("const", 7)); // all-equal leaves, bitset ids
        if (d % 10 == 0) { // negative and wide values, two per doc, 16-bit delta ids
          doc.add(new LongPoint("sparse", -d));
          doc.add(new LongPoint("sparse", d * 1_000_003L));
        }
        w.addDocument(doc);
      }
      w.forceMerge(1);
    }
  }
}
