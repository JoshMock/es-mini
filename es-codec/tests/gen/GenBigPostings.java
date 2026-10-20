import java.nio.file.*;
import org.apache.lucene.analysis.core.WhitespaceAnalyzer;
import org.apache.lucene.document.*;
import org.apache.lucene.index.*;
import org.apache.lucene.store.*;

/** Writes the index read by tests/big_postings.rs. Run via gen.sh; see that test for the contents. */
public class GenBigPostings {
  static final int DOCS = 8500; // more than 256 * 32, so postings get a second skip level

  static String repeat(String t, int n) {
    return (t + " ").repeat(n);
  }

  public static void main(String[] args) throws Exception {
    Path out = Paths.get(args[0]);
    try (Directory dir = FSDirectory.open(out);
        IndexWriter w = new IndexWriter(dir, new IndexWriterConfig(new WhitespaceAnalyzer()).setUseCompoundFile(true))) {
      FieldType docsOnly = new FieldType(StringField.TYPE_NOT_STORED);
      FieldType positions = new FieldType(TextField.TYPE_NOT_STORED);
      for (int d = 0; d < DOCS; d++) {
        StringBuilder text = new StringBuilder(repeat("all", d % 5 == 0 ? 8 : 1));
        if (d % 7 == 0) text.append(repeat("seventh", d % 13 == 0 ? 40 : 1));
        if (d < 300) text.append(repeat("common", 1 + d % 4));
        if (d % 3 != 0) text.append("dense ");
        if (d == 4242) text.append("single");
        Document doc = new Document();
        doc.add(new Field("text", text.toString(), positions));
        doc.add(new Field("key", "all", docsOnly));
        if (d % 7 == 0) doc.add(new Field("key", "seventh", docsOnly));
        if (d < 300) doc.add(new Field("key", "common", docsOnly));
        if (d % 3 != 0) doc.add(new Field("key", "dense", docsOnly));
        w.addDocument(doc);
      }
      w.forceMerge(1);
    }
  }
}
