import java.nio.file.*;
import org.apache.lucene.analysis.standard.StandardAnalyzer;
import org.apache.lucene.document.*;
import org.apache.lucene.index.*;
import org.apache.lucene.search.*;
import org.apache.lucene.store.*;
import org.apache.lucene.util.BytesRef;

/**
 * Writes the index read by tests/index_sort.rs: an index sorted by the three kinds of sort field
 * Lucene serializes, in one flushed (compound)
 * segment. The value formulas must match the ones there.
 */
public class GenIndexSort {
  static final int DOCS = 100;

  public static void main(String[] args) throws Exception {
    Sort sort =
        new Sort(
            new SortedSetSortField("tag", false, SortedSetSelector.Type.MIN, SortField.STRING_FIRST),
            new SortedNumericSortField("ts", SortField.Type.LONG, true, SortedNumericSelector.Type.MAX, Long.MIN_VALUE),
            new SortField("score", SortField.Type.FLOAT, false, 0.25f),
            new SortField("id", SortField.Type.LONG));
    IndexWriterConfig config = new IndexWriterConfig(new StandardAnalyzer()).setIndexSort(sort);
    try (Directory dir = FSDirectory.open(Paths.get(args[0]));
        IndexWriter w = new IndexWriter(dir, config)) {
      for (int d = 0; d < DOCS; d++) {
        Document doc = new Document();
        doc.add(new StoredField("id", d));
        doc.add(new NumericDocValuesField("id", d));
        if (d % 10 != 0) doc.add(new SortedSetDocValuesField("tag", new BytesRef("t" + d % 3)));
        if (d % 7 != 0) {
          doc.add(new SortedNumericDocValuesField("ts", d % 6 * 1000L));
          if (d % 2 == 0) doc.add(new SortedNumericDocValuesField("ts", d % 6 * 1000L + 500));
        }
        if (d % 3 != 0) doc.add(new FloatDocValuesField("score", d % 9 * 0.5f - 1f));
        w.addDocument(doc);
      }
    }
  }
}
