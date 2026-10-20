import java.nio.file.*;
import org.apache.lucene.analysis.*;
import org.apache.lucene.analysis.core.WhitespaceTokenizer;
import org.apache.lucene.analysis.payloads.*;
import org.apache.lucene.document.*;
import org.apache.lucene.index.*;
import org.apache.lucene.store.*;

/** Writes the index read by tests/positions.rs; the doc text must match `text()` there. */
public class GenPositions {
  static final int DOCS = 600;

  static String text(int d, boolean payloads) {
    int w = 1 + d % 5;
    int n = w + (d % 7 == 0 ? 300 : 0) + (d % 3 == 0 ? 1 : 0);
    StringBuilder sb = new StringBuilder();
    for (int j = 0; j < n; j++) {
      String tok = j < w ? "w" : (j < n - (d % 3 == 0 ? 1 : 0) ? "x" : "y");
      if (payloads && (d + j) % 4 != 0) tok += String.format("|%02d", (d + j) % 100);
      if (j > 0) sb.append(' ');
      sb.append(tok);
    }
    return sb.toString();
  }

  public static void main(String[] args) throws Exception {
    Analyzer analyzer = new Analyzer() {
      @Override
      protected TokenStreamComponents createComponents(String field) {
        Tokenizer t = new WhitespaceTokenizer();
        return new TokenStreamComponents(t, new DelimitedPayloadTokenFilter(t, '|', new IdentityEncoder()));
      }
    };
    try (Directory dir = FSDirectory.open(Paths.get(args[0]));
        IndexWriter w = new IndexWriter(dir, new IndexWriterConfig(analyzer).setUseCompoundFile(true))) {
      String[] names = {"plain", "offsets", "payloads", "both"};
      IndexOptions[] options = {
        IndexOptions.DOCS_AND_FREQS_AND_POSITIONS,
        IndexOptions.DOCS_AND_FREQS_AND_POSITIONS_AND_OFFSETS,
        IndexOptions.DOCS_AND_FREQS_AND_POSITIONS,
        IndexOptions.DOCS_AND_FREQS_AND_POSITIONS_AND_OFFSETS
      };
      for (int d = 0; d < DOCS; d++) {
        Document doc = new Document();
        for (int f = 0; f < names.length; f++) {
          FieldType type = new FieldType(TextField.TYPE_NOT_STORED);
          type.setIndexOptions(options[f]);
          doc.add(new Field(names[f], text(d, f >= 2), type));
        }
        w.addDocument(doc);
      }
      w.forceMerge(1);
    }
  }
}
