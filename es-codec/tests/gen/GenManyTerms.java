import java.nio.file.*;
import java.util.*;
import org.apache.lucene.analysis.standard.StandardAnalyzer;
import org.apache.lucene.document.*;
import org.apache.lucene.index.*;
import org.apache.lucene.store.*;

/** Writes the index read by tests/many_terms.rs; `terms()` must match the test's. Doc d holds term d. */
public class GenManyTerms {
  static List<String> terms() {
    List<String> out = new ArrayList<>();
    // Repetitive long suffixes behind a shared prefix: LZ4-compressed blocks.
    for (int i = 0; i < 60; i++) out.add(String.format("lz4%03d-abcdefabcdefabcdef", i));
    // Random lowercase suffixes behind a shared prefix: lowercase-ASCII-compressed blocks.
    long x = 42;
    for (int i = 0; i < 60; i++) {
      StringBuilder sb = new StringBuilder("low");
      for (int j = 0; j < 8; j++) {
        x = (x * 1103515245L + 12345L) & 0x7fffffffL;
        sb.append((char) ('a' + (int) (x >> 16) % 26));
      }
      out.add(sb.append(i).toString());
    }
    // 400 terms under one prefix, 100 per next byte: sub-blocks split into floor blocks.
    for (int i = 0; i < 400; i++) out.add(String.format("flo%03d", i));
    // Short, unrelated terms: uncompressed blocks.
    for (int i = 0; i < 100; i++) out.add(Integer.toString(i * 37, 36));
    return out;
  }

  public static void main(String[] args) throws Exception {
    try (Directory dir = FSDirectory.open(Paths.get(args[0]));
        IndexWriter w = new IndexWriter(dir, new IndexWriterConfig(new StandardAnalyzer()).setUseCompoundFile(true))) {
      for (String t : terms()) {
        Document doc = new Document();
        doc.add(new StringField("t", t, Field.Store.NO));
        w.addDocument(doc);
      }
      w.forceMerge(1);
    }
  }
}
