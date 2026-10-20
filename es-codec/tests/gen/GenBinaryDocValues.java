import java.nio.file.*;
import org.apache.lucene.analysis.standard.StandardAnalyzer;
import org.apache.lucene.document.*;
import org.apache.lucene.index.*;
import org.apache.lucene.store.*;
import org.apache.lucene.util.BytesRef;

/** Writes the index read by tests/binary_doc_values.rs; the values must match `value` there. */
public class GenBinaryDocValues {
  static final int DOCS = 300;

  static byte[] value(String field, int d) {
    switch (field) {
      case "fixed": // every doc, 8 bytes
        return java.nio.ByteBuffer.allocate(8).putLong(d * 0x0001_0101_0101L).array();
      case "sparse_fixed": // every tenth doc, 4 bytes
        return d % 10 == 0 ? java.nio.ByteBuffer.allocate(4).putInt(d).array() : null;
      case "varlen": // two of three docs, 0 to 16 bytes
        if (d % 3 == 0) return null;
        byte[] b = new byte[d % 17];
        for (int i = 0; i < b.length; i++) b[i] = (byte) (d * 31 + i);
        return b;
      case "dense_varlen": // every doc, 1 to 5 bytes
        byte[] c = new byte[1 + d % 5];
        for (int i = 0; i < c.length; i++) c[i] = (byte) (d + i);
        return c;
      default:
        throw new IllegalArgumentException(field);
    }
  }

  public static void main(String[] args) throws Exception {
    try (Directory dir = FSDirectory.open(Paths.get(args[0]));
        IndexWriter w = new IndexWriter(dir, new IndexWriterConfig(new StandardAnalyzer()).setUseCompoundFile(true))) {
      for (int d = 0; d < DOCS; d++) {
        Document doc = new Document();
        for (String f : new String[] {"fixed", "sparse_fixed", "varlen", "dense_varlen"}) {
          byte[] v = value(f, d);
          if (v != null) doc.add(new BinaryDocValuesField(f, new BytesRef(v)));
        }
        w.addDocument(doc);
      }
      w.forceMerge(1);
    }
  }
}
