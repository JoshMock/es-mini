import java.nio.file.*;
import org.apache.lucene.analysis.standard.StandardAnalyzer;
import org.apache.lucene.document.*;
import org.apache.lucene.index.*;
import org.apache.lucene.store.*;

/**
 * Writes the index read by tests/stored_and_numeric.rs; the value formulas must match the test's.
 * 70000 docs so doc values span two 65536-doc DISI blocks; then hard deletes, so a .liv file.
 */
public class GenStoredAndNumeric {
  static final int DOCS = 70000;
  static final int STORED_DOCS = 3000; // three chunks of 1024 docs, plus a huge doc
  static final int HUGE_DOC = 1500;
  static final long DAY = 86_400_000L;

  static float f(int d) {
    switch (d % 4) {
      case 0: return -1.5f * d; // negative
      case 1: return d % 100; // small integer
      case 2: return d + 0.25f; // positive float
      default: return 0f;
    }
  }

  static double dbl(int d) {
    switch (d % 4) {
      case 0: return -d * 1.125; // negative
      case 1: return d % 100; // small integer
      case 2: return d + 0.5; // exactly a float
      default: return d + 0.1; // positive double
    }
  }

  static long lng(int d) {
    switch (d % 5) {
      case 0: return d * DAY; // day-encoded
      case 1: return d * 3_600_000L; // hour-encoded
      case 2: return d * 1000L; // second-encoded
      case 3: return -d * 7L; // raw, negative
      default: return Long.MAX_VALUE - d; // raw, needs a vlong continuation
    }
  }

  public static void main(String[] args) throws Exception {
    Path out = Paths.get(args[0]);
    try (Directory dir = FSDirectory.open(out)) {
      try (IndexWriter w = new IndexWriter(dir, new IndexWriterConfig(new StandardAnalyzer()).setUseCompoundFile(true))) {
        for (int d = 0; d < DOCS; d++) {
          Document doc = new Document();
          if (d < STORED_DOCS) {
            doc.add(new StoredField("int", d % 2 == 0 ? d : -d));
            doc.add(new StoredField("float", f(d)));
            doc.add(new StoredField("long", lng(d)));
            doc.add(new StoredField("double", dbl(d)));
            doc.add(new StoredField("string", d == HUGE_DOC ? "x".repeat(200_000) : "s" + d));
            doc.add(new StoredField("bytes", new byte[] {(byte) d, (byte) (d >> 8)}));
          }
          if (d % 2 == 0) doc.add(new NumericDocValuesField("half", d % 8)); // dense block, then sparse
          if (d < 65536 || d % 9 == 0) doc.add(new NumericDocValuesField("head", 1)); // full block
          // Small values in the first 16384-value block, wider ones in the second: varying bits per
          // value.
          if (d < 32768) doc.add(new NumericDocValuesField("varying", d < 16384 ? d % 4 : d % 4096));
          doc.add(new SortedNumericDocValuesField("multi", d % 4));
          if (d % 3 == 0) doc.add(new SortedNumericDocValuesField("multi", -(d % 4)));
          if (d % 1000 == 7) doc.add(new StringField("del", "y", Field.Store.NO));
          w.addDocument(doc);
        }
        w.forceMerge(1);
      }
      try (IndexWriter w = new IndexWriter(dir, new IndexWriterConfig(new StandardAnalyzer()).setMergePolicy(NoMergePolicy.INSTANCE))) {
        w.deleteDocuments(new Term("del", "y"));
      }
    }
  }
}
