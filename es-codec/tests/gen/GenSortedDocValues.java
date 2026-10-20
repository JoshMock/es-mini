import java.nio.file.*;
import org.apache.lucene.analysis.standard.StandardAnalyzer;
import org.apache.lucene.document.*;
import org.apache.lucene.index.*;
import org.apache.lucene.store.*;
import org.apache.lucene.util.BytesRef;

/** Writes the index read by tests/sorted_doc_values.rs; the terms must match `terms` there. */
public class GenSortedDocValues {
  static final int DOCS = 300;

  static String[] terms(String field, int d) {
    switch (field) {
      case "dense": // 150 terms sharing a long prefix, so several term blocks
        return new String[] {"a-common-prefix-longer-than-fifteen-bytes-" + String.format("%03d", d % 150)};
      case "sparse": // every fourth doc; terms with suffixes longer than 16 bytes
        return d % 4 == 0 ? new String[] {"s".repeat(20 + d % 7) + (d % 3)} : null;
      case "multi": // sorted set, up to two values per doc
        return d % 5 == 0 ? null : new String[] {String.format("m%03d", d % 100), String.format("m%03d", d * 7 % 100)};
      default:
        throw new IllegalArgumentException(field);
    }
  }

  public static void main(String[] args) throws Exception {
    try (Directory dir = FSDirectory.open(Paths.get(args[0]));
        IndexWriter w = new IndexWriter(dir, new IndexWriterConfig(new StandardAnalyzer()).setUseCompoundFile(true))) {
      for (int d = 0; d < DOCS; d++) {
        Document doc = new Document();
        for (String f : new String[] {"dense", "sparse", "multi"}) {
          String[] ts = terms(f, d);
          if (ts == null) continue;
          for (String t : ts) {
            doc.add(f.equals("multi") ? new SortedSetDocValuesField(f, new BytesRef(t)) : new SortedDocValuesField(f, new BytesRef(t)));
          }
        }
        w.addDocument(doc);
      }
      w.forceMerge(1);
    }
  }
}
