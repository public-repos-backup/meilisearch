use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::num::NonZero;
use std::ops::DerefMut as _;

use bumpalo::Bump;
use grenad::{Merger, MergerBuilder};
use heed::RoTxn;

use super::tokenize_document::{tokenizer_builder, DocumentTokenizer};
use crate::update::new::extract::cache::CboCachedSorter;
use crate::update::new::extract::perm_json_p::contained_in;
use crate::update::new::indexer::document_changes::{
    for_each_document_change, DocumentChangeContext, DocumentChanges, Extractor, FullySend,
    IndexingContext, RefCellExt, ThreadLocal,
};
use crate::update::new::DocumentChange;
use crate::update::{create_sorter, GrenadParameters, MergeDeladdCboRoaringBitmaps};
use crate::{bucketed_position, DocumentId, FieldId, Index, Result, MAX_POSITION_PER_ATTRIBUTE};

const MAX_COUNTED_WORDS: usize = 30;

pub struct WordDocidsCachedSorters {
    word_fid_docids: CboCachedSorter<MergeDeladdCboRoaringBitmaps>,
    word_docids: CboCachedSorter<MergeDeladdCboRoaringBitmaps>,
    exact_word_docids: CboCachedSorter<MergeDeladdCboRoaringBitmaps>,
    word_position_docids: CboCachedSorter<MergeDeladdCboRoaringBitmaps>,
    fid_word_count_docids: CboCachedSorter<MergeDeladdCboRoaringBitmaps>,
    fid_word_count: HashMap<FieldId, (usize, usize)>,
    current_docid: Option<DocumentId>,
}

impl WordDocidsCachedSorters {
    pub fn new(
        indexer: GrenadParameters,
        max_memory: Option<usize>,
        capacity: NonZero<usize>,
    ) -> Self {
        let max_memory = max_memory.map(|max_memory| max_memory / 4);

        let word_fid_docids = CboCachedSorter::new(
            capacity,
            create_sorter(
                grenad::SortAlgorithm::Stable,
                MergeDeladdCboRoaringBitmaps,
                indexer.chunk_compression_type,
                indexer.chunk_compression_level,
                indexer.max_nb_chunks,
                max_memory,
                false,
            ),
        );
        let word_docids = CboCachedSorter::new(
            capacity,
            create_sorter(
                grenad::SortAlgorithm::Stable,
                MergeDeladdCboRoaringBitmaps,
                indexer.chunk_compression_type,
                indexer.chunk_compression_level,
                indexer.max_nb_chunks,
                max_memory,
                false,
            ),
        );
        let exact_word_docids = CboCachedSorter::new(
            capacity,
            create_sorter(
                grenad::SortAlgorithm::Stable,
                MergeDeladdCboRoaringBitmaps,
                indexer.chunk_compression_type,
                indexer.chunk_compression_level,
                indexer.max_nb_chunks,
                max_memory,
                false,
            ),
        );
        let word_position_docids = CboCachedSorter::new(
            capacity,
            create_sorter(
                grenad::SortAlgorithm::Stable,
                MergeDeladdCboRoaringBitmaps,
                indexer.chunk_compression_type,
                indexer.chunk_compression_level,
                indexer.max_nb_chunks,
                max_memory,
                false,
            ),
        );
        let fid_word_count_docids = CboCachedSorter::new(
            capacity,
            create_sorter(
                grenad::SortAlgorithm::Stable,
                MergeDeladdCboRoaringBitmaps,
                indexer.chunk_compression_type,
                indexer.chunk_compression_level,
                indexer.max_nb_chunks,
                max_memory,
                false,
            ),
        );

        Self {
            word_fid_docids,
            word_docids,
            exact_word_docids,
            word_position_docids,
            fid_word_count_docids,
            fid_word_count: HashMap::new(),
            current_docid: None,
        }
    }

    fn insert_add_u32(
        &mut self,
        field_id: FieldId,
        position: u16,
        word: &str,
        exact: bool,
        docid: u32,
        buffer: &mut Vec<u8>,
    ) -> Result<()> {
        let key = word.as_bytes();
        if exact {
            self.exact_word_docids.insert_add_u32(key, docid)?;
        } else {
            self.word_docids.insert_add_u32(key, docid)?;
        }

        buffer.clear();
        buffer.extend_from_slice(word.as_bytes());
        buffer.push(0);
        buffer.extend_from_slice(&field_id.to_be_bytes());
        self.word_fid_docids.insert_add_u32(buffer, docid)?;

        let position = bucketed_position(position);
        buffer.clear();
        buffer.extend_from_slice(word.as_bytes());
        buffer.push(0);
        buffer.extend_from_slice(&position.to_be_bytes());
        self.word_position_docids.insert_add_u32(buffer, docid)?;

        if self.current_docid.map_or(false, |id| docid != id) {
            self.flush_fid_word_count(buffer)?;
        }

        self.fid_word_count
            .entry(field_id)
            .and_modify(|(_current_count, new_count)| *new_count += 1)
            .or_insert((0, 1));
        self.current_docid = Some(docid);

        Ok(())
    }

    fn insert_del_u32(
        &mut self,
        field_id: FieldId,
        position: u16,
        word: &str,
        exact: bool,
        docid: u32,
        buffer: &mut Vec<u8>,
    ) -> Result<()> {
        let key = word.as_bytes();
        if exact {
            self.exact_word_docids.insert_del_u32(key, docid)?;
        } else {
            self.word_docids.insert_del_u32(key, docid)?;
        }

        buffer.clear();
        buffer.extend_from_slice(word.as_bytes());
        buffer.push(0);
        buffer.extend_from_slice(&field_id.to_be_bytes());
        self.word_fid_docids.insert_del_u32(buffer, docid)?;

        let position = bucketed_position(position);
        buffer.clear();
        buffer.extend_from_slice(word.as_bytes());
        buffer.push(0);
        buffer.extend_from_slice(&position.to_be_bytes());
        self.word_position_docids.insert_del_u32(buffer, docid)?;

        if self.current_docid.map_or(false, |id| docid != id) {
            self.flush_fid_word_count(buffer)?;
        }

        self.fid_word_count
            .entry(field_id)
            .and_modify(|(current_count, _new_count)| *current_count += 1)
            .or_insert((1, 0));
        self.current_docid = Some(docid);

        Ok(())
    }

    fn flush_fid_word_count(&mut self, buffer: &mut Vec<u8>) -> Result<()> {
        for (fid, (current_count, new_count)) in self.fid_word_count.drain() {
            if current_count != new_count {
                if current_count <= MAX_COUNTED_WORDS {
                    buffer.clear();
                    buffer.extend_from_slice(&fid.to_be_bytes());
                    buffer.push(current_count as u8);
                    self.fid_word_count_docids
                        .insert_del_u32(buffer, self.current_docid.unwrap())?;
                }
                if new_count <= MAX_COUNTED_WORDS {
                    buffer.clear();
                    buffer.extend_from_slice(&fid.to_be_bytes());
                    buffer.push(new_count as u8);
                    self.fid_word_count_docids
                        .insert_add_u32(buffer, self.current_docid.unwrap())?;
                }
            }
        }

        Ok(())
    }
}

struct WordDocidsMergerBuilders {
    word_fid_docids: MergerBuilder<File, MergeDeladdCboRoaringBitmaps>,
    word_docids: MergerBuilder<File, MergeDeladdCboRoaringBitmaps>,
    exact_word_docids: MergerBuilder<File, MergeDeladdCboRoaringBitmaps>,
    word_position_docids: MergerBuilder<File, MergeDeladdCboRoaringBitmaps>,
    fid_word_count_docids: MergerBuilder<File, MergeDeladdCboRoaringBitmaps>,
}

pub struct WordDocidsMergers {
    pub word_fid_docids: Merger<File, MergeDeladdCboRoaringBitmaps>,
    pub word_docids: Merger<File, MergeDeladdCboRoaringBitmaps>,
    pub exact_word_docids: Merger<File, MergeDeladdCboRoaringBitmaps>,
    pub word_position_docids: Merger<File, MergeDeladdCboRoaringBitmaps>,
    pub fid_word_count_docids: Merger<File, MergeDeladdCboRoaringBitmaps>,
}

impl WordDocidsMergerBuilders {
    fn new() -> Self {
        Self {
            word_fid_docids: MergerBuilder::new(MergeDeladdCboRoaringBitmaps),
            word_docids: MergerBuilder::new(MergeDeladdCboRoaringBitmaps),
            exact_word_docids: MergerBuilder::new(MergeDeladdCboRoaringBitmaps),
            word_position_docids: MergerBuilder::new(MergeDeladdCboRoaringBitmaps),
            fid_word_count_docids: MergerBuilder::new(MergeDeladdCboRoaringBitmaps),
        }
    }

    fn add_sorters(&mut self, other: WordDocidsCachedSorters) -> Result<()> {
        let WordDocidsCachedSorters {
            word_fid_docids,
            word_docids,
            exact_word_docids,
            word_position_docids,
            fid_word_count_docids,
            fid_word_count: _,
            current_docid: _,
        } = other;

        let mut word_fid_docids_readers = Ok(vec![]);
        let mut word_docids_readers = Ok(vec![]);
        let mut exact_word_docids_readers = Ok(vec![]);
        let mut word_position_docids_readers = Ok(vec![]);
        let mut fid_word_count_docids_readers = Ok(vec![]);
        rayon::scope(|s| {
            s.spawn(|_| {
                word_fid_docids_readers =
                    word_fid_docids.into_sorter().and_then(|s| s.into_reader_cursors());
            });
            s.spawn(|_| {
                word_docids_readers =
                    word_docids.into_sorter().and_then(|s| s.into_reader_cursors());
            });
            s.spawn(|_| {
                exact_word_docids_readers =
                    exact_word_docids.into_sorter().and_then(|s| s.into_reader_cursors());
            });
            s.spawn(|_| {
                word_position_docids_readers =
                    word_position_docids.into_sorter().and_then(|s| s.into_reader_cursors());
            });
            s.spawn(|_| {
                fid_word_count_docids_readers =
                    fid_word_count_docids.into_sorter().and_then(|s| s.into_reader_cursors());
            });
        });
        self.word_fid_docids.extend(word_fid_docids_readers?);
        self.word_docids.extend(word_docids_readers?);
        self.exact_word_docids.extend(exact_word_docids_readers?);
        self.word_position_docids.extend(word_position_docids_readers?);
        self.fid_word_count_docids.extend(fid_word_count_docids_readers?);

        Ok(())
    }

    fn build(self) -> WordDocidsMergers {
        WordDocidsMergers {
            word_fid_docids: self.word_fid_docids.build(),
            word_docids: self.word_docids.build(),
            exact_word_docids: self.exact_word_docids.build(),
            word_position_docids: self.word_position_docids.build(),
            fid_word_count_docids: self.fid_word_count_docids.build(),
        }
    }
}

pub struct WordDocidsExtractorData<'extractor> {
    tokenizer: &'extractor DocumentTokenizer<'extractor>,
    grenad_parameters: GrenadParameters,
    max_memory: Option<usize>,
}

impl<'extractor> Extractor<'extractor> for WordDocidsExtractorData<'extractor> {
    type Data = FullySend<RefCell<WordDocidsCachedSorters>>;

    fn init_data(
        &self,
        _extractor_alloc: raw_collections::alloc::RefBump<'extractor>,
    ) -> Result<Self::Data> {
        Ok(FullySend(RefCell::new(WordDocidsCachedSorters::new(
            self.grenad_parameters,
            self.max_memory,
            // TODO use a better value
            200_000.try_into().unwrap(),
        ))))
    }

    fn process<'doc>(
        &self,
        changes: impl Iterator<Item = Result<DocumentChange<'doc>>>,
        context: &DocumentChangeContext<Self::Data>,
    ) -> Result<()> {
        for change in changes {
            let change = change?;
            WordDocidsExtractors::extract_document_change(context, self.tokenizer, change)?;
        }
        Ok(())
    }
}

pub struct WordDocidsExtractors;

impl WordDocidsExtractors {
    pub fn run_extraction<'pl, 'fid, 'indexer, 'index, DC: DocumentChanges<'pl>>(
        grenad_parameters: GrenadParameters,
        document_changes: &DC,
        indexing_context: IndexingContext<'fid, 'indexer, 'index>,
        extractor_allocs: &mut ThreadLocal<FullySend<RefCell<Bump>>>,
    ) -> Result<WordDocidsMergers> {
        let max_memory = grenad_parameters.max_memory_by_thread();
        let index = indexing_context.index;

        let rtxn = index.read_txn()?;
        let stop_words = index.stop_words(&rtxn)?;
        let allowed_separators = index.allowed_separators(&rtxn)?;
        let allowed_separators: Option<Vec<_>> =
            allowed_separators.as_ref().map(|s| s.iter().map(String::as_str).collect());
        let dictionary = index.dictionary(&rtxn)?;
        let dictionary: Option<Vec<_>> =
            dictionary.as_ref().map(|s| s.iter().map(String::as_str).collect());
        let builder = tokenizer_builder(
            stop_words.as_ref(),
            allowed_separators.as_deref(),
            dictionary.as_deref(),
        );
        let tokenizer = builder.into_tokenizer();

        let attributes_to_extract = Self::attributes_to_extract(&rtxn, index)?;
        let attributes_to_skip = Self::attributes_to_skip(&rtxn, index)?;
        let localized_attributes_rules =
            index.localized_attributes_rules(&rtxn)?.unwrap_or_default();

        let document_tokenizer = DocumentTokenizer {
            tokenizer: &tokenizer,
            attribute_to_extract: attributes_to_extract.as_deref(),
            attribute_to_skip: attributes_to_skip.as_slice(),
            localized_attributes_rules: &localized_attributes_rules,
            max_positions_per_attributes: MAX_POSITION_PER_ATTRIBUTE,
        };

        let datastore = ThreadLocal::new();

        {
            let span =
                tracing::trace_span!(target: "indexing::documents::extract", "docids_extraction");
            let _entered = span.enter();

            let extractor = WordDocidsExtractorData {
                tokenizer: &document_tokenizer,
                grenad_parameters,
                max_memory,
            };

            for_each_document_change(
                document_changes,
                &extractor,
                indexing_context,
                extractor_allocs,
                &datastore,
            )?;
        }

        {
            let span =
                tracing::trace_span!(target: "indexing::documents::extract", "merger_building");
            let _entered = span.enter();
            let mut builder = WordDocidsMergerBuilders::new();
            for cache in datastore.into_iter().map(|cache| cache.0.into_inner()) {
                builder.add_sorters(cache)?;
            }

            Ok(builder.build())
        }
    }

    fn extract_document_change(
        context: &DocumentChangeContext<FullySend<RefCell<WordDocidsCachedSorters>>>,
        document_tokenizer: &DocumentTokenizer,
        document_change: DocumentChange,
    ) -> Result<()> {
        let index = &context.index;
        let rtxn = &context.txn;
        let mut cached_sorter = context.data.0.borrow_mut_or_yield();
        let cached_sorter = cached_sorter.deref_mut();
        let mut new_fields_ids_map = context.new_fields_ids_map.borrow_mut_or_yield();
        let new_fields_ids_map = new_fields_ids_map.deref_mut();

        let exact_attributes = index.exact_attributes(rtxn)?;
        let is_exact_attribute =
            |fname: &str| exact_attributes.iter().any(|attr| contained_in(fname, attr));
        let mut buffer = Vec::new();
        match document_change {
            DocumentChange::Deletion(inner) => {
                let mut token_fn = |fname: &str, fid, pos, word: &str| {
                    cached_sorter
                        .insert_del_u32(
                            fid,
                            pos,
                            word,
                            is_exact_attribute(fname),
                            inner.docid(),
                            &mut buffer,
                        )
                        .map_err(crate::Error::from)
                };
                document_tokenizer.tokenize_document(
                    inner.current(rtxn, index, context.db_fields_ids_map)?,
                    new_fields_ids_map,
                    &mut token_fn,
                )?;
            }
            DocumentChange::Update(inner) => {
                let mut token_fn = |fname: &str, fid, pos, word: &str| {
                    cached_sorter
                        .insert_del_u32(
                            fid,
                            pos,
                            word,
                            is_exact_attribute(fname),
                            inner.docid(),
                            &mut buffer,
                        )
                        .map_err(crate::Error::from)
                };
                document_tokenizer.tokenize_document(
                    inner.current(rtxn, index, context.db_fields_ids_map)?,
                    new_fields_ids_map,
                    &mut token_fn,
                )?;

                let mut token_fn = |fname: &str, fid, pos, word: &str| {
                    cached_sorter
                        .insert_add_u32(
                            fid,
                            pos,
                            word,
                            is_exact_attribute(fname),
                            inner.docid(),
                            &mut buffer,
                        )
                        .map_err(crate::Error::from)
                };
                document_tokenizer.tokenize_document(
                    inner.new(rtxn, index, context.db_fields_ids_map)?,
                    new_fields_ids_map,
                    &mut token_fn,
                )?;
            }
            DocumentChange::Insertion(inner) => {
                let mut token_fn = |fname: &str, fid, pos, word: &str| {
                    cached_sorter
                        .insert_add_u32(
                            fid,
                            pos,
                            word,
                            is_exact_attribute(fname),
                            inner.docid(),
                            &mut buffer,
                        )
                        .map_err(crate::Error::from)
                };
                document_tokenizer.tokenize_document(
                    inner.new(),
                    new_fields_ids_map,
                    &mut token_fn,
                )?;
            }
        }

        cached_sorter.flush_fid_word_count(&mut buffer)
    }

    fn attributes_to_extract<'a>(
        rtxn: &'a RoTxn,
        index: &'a Index,
    ) -> Result<Option<Vec<&'a str>>> {
        index.user_defined_searchable_fields(rtxn).map_err(Into::into)
    }

    fn attributes_to_skip<'a>(_rtxn: &'a RoTxn, _index: &'a Index) -> Result<Vec<&'a str>> {
        Ok(vec![])
    }
}
