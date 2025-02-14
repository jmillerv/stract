// Stract is an open source web search engine.
// Copyright (C) 2023 Stract ApS
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

//! The inverted index is the main data structure of the search engine.
//! It is a mapping from terms to a list of documents. Imagine a hash map
//! { term -> \[doc1, doc2, doc3\] } etc. During search, we look up the terms
//! from the query in the index and perform an intersection of the lists of
//! documents. The result is a list of documents that match the query which
//! is then ranked.
//!
//! The inverted index is implemented using tantivy. The inverted index in
//! tantivy is actually a FST (finite state transducer) and not a hash map.
//! This allows us to perform more advanced queries than just term lookups,
//! but the principle is the same.
use chrono::NaiveDateTime;
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use tantivy::collector::Count;
use tantivy::directory::MmapDirectory;
use tantivy::merge_policy::NoMergePolicy;
use tantivy::schema::{Schema, Value};
use tantivy::tokenizer::TokenizerManager;
use tantivy::{IndexReader, IndexWriter, SegmentMeta, TantivyDocument};
use url::Url;

use crate::collector::{Hashes, MainCollector};
use crate::config::SnippetConfig;
use crate::fastfield_reader::FastFieldReader;
use crate::query::shortcircuit::ShortCircuitQuery;
use crate::query::Query;
use crate::rake::RakeModel;
use crate::ranking::initial::Score;
use crate::ranking::pipeline::RankingWebsite;
use crate::ranking::SignalAggregator;
use crate::schema::{FastField, Field, TextField};
use crate::search_ctx::Ctx;
use crate::snippet::TextSnippet;
use crate::snippet::{self, TextSnippetFragment};
use crate::tokenizer::{
    BigramTokenizer, Identity, JsonField, SiteOperatorUrlTokenizer, TrigramTokenizer,
};
use crate::webgraph::NodeID;
use crate::webpage::region::Region;
use crate::webpage::url_ext::UrlExt;
use crate::webpage::{schema_org, Webpage};
use crate::Result;
use crate::{schema::create_schema, tokenizer::Tokenizer};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Serialize, Deserialize)]
pub struct InitialSearchResult {
    pub num_websites: Option<usize>,
    pub top_websites: Vec<WebsitePointer>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct WebsitePointer {
    pub score: Score,
    pub hashes: Hashes,
    pub address: DocAddress,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq)]
pub struct DocAddress {
    pub segment: u32,
    pub doc_id: u32,
}

impl From<tantivy::DocAddress> for DocAddress {
    fn from(address: tantivy::DocAddress) -> Self {
        Self {
            segment: address.segment_ord,
            doc_id: address.doc_id,
        }
    }
}

impl From<DocAddress> for tantivy::DocAddress {
    fn from(address: DocAddress) -> Self {
        Self {
            segment_ord: address.segment,
            doc_id: address.doc_id,
        }
    }
}

pub fn merge_tantivy_segments<P: AsRef<Path>>(
    writer: &mut IndexWriter,
    mut segments: Vec<SegmentMeta>,
    base_path: P,
    max_num_segments: u64,
) -> Result<()> {
    assert!(max_num_segments > 0);

    if segments.len() <= max_num_segments as usize {
        return Ok(());
    }

    let num_segments = (max_num_segments + 1) / 2; // ceil(num_segments/2)

    let mut merge_segments = Vec::new();

    for _ in 0..num_segments {
        merge_segments.push(SegmentMergeCandidate {
            num_docs: 0,
            segments: Vec::new(),
        });
    }

    segments.sort_by_key(|b| std::cmp::Reverse(b.num_docs()));

    for segment in segments {
        let best_candidate = merge_segments
            .iter_mut()
            .min_by(|a, b| a.num_docs.cmp(&b.num_docs))
            .unwrap();

        best_candidate.num_docs += segment.num_docs();
        best_candidate.segments.push(segment);
    }

    for merge in merge_segments
        .into_iter()
        .filter(|merge| !merge.segments.is_empty())
    {
        let segment_ids: Vec<_> = merge.segments.iter().map(|segment| segment.id()).collect();
        writer.merge(&segment_ids[..]).wait()?;

        for segment in merge.segments {
            for file in segment.list_files() {
                std::fs::remove_file(base_path.as_ref().join(file)).ok();
            }
        }
    }

    Ok(())
}

struct SegmentMergeCandidate {
    num_docs: u32,
    segments: Vec<SegmentMeta>,
}

pub struct InvertedIndex {
    pub path: String,
    tantivy_index: tantivy::Index,
    writer: Option<IndexWriter>,
    reader: IndexReader,
    schema: Arc<Schema>,
    snippet_config: SnippetConfig,
    fastfield_reader: FastFieldReader,
    rake: RakeModel,
}

impl InvertedIndex {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let schema = create_schema();

        let tantivy_index = if path.as_ref().exists() {
            let mmap_directory = MmapDirectory::open(&path)?;
            tantivy::Index::open(mmap_directory)?
        } else {
            let index_settings = tantivy::IndexSettings {
                sort_by_field: Some(tantivy::IndexSortByField {
                    field: Field::Fast(FastField::PreComputedScore).name().to_string(),
                    order: tantivy::Order::Desc,
                }),
                ..Default::default()
            };

            fs::create_dir_all(&path)?;
            let mmap_directory = MmapDirectory::open(&path)?;
            tantivy::Index::create(mmap_directory, schema.clone(), index_settings)?
        };

        let tokenizer = Tokenizer::default();
        tantivy_index
            .tokenizers()
            .register(tokenizer.as_str(), tokenizer);

        let tokenizer = Tokenizer::new_stemmed();
        tantivy_index
            .tokenizers()
            .register(tokenizer.as_str(), tokenizer);

        let tokenizer = Tokenizer::Identity(Identity::default());
        tantivy_index
            .tokenizers()
            .register(tokenizer.as_str(), tokenizer);

        let tokenizer = Tokenizer::Bigram(BigramTokenizer::default());
        tantivy_index
            .tokenizers()
            .register(tokenizer.as_str(), tokenizer);

        let tokenizer = Tokenizer::Trigram(TrigramTokenizer::default());
        tantivy_index
            .tokenizers()
            .register(tokenizer.as_str(), tokenizer);

        let tokenizer = Tokenizer::SiteOperator(SiteOperatorUrlTokenizer);
        tantivy_index
            .tokenizers()
            .register(tokenizer.as_str(), tokenizer);

        let tokenizer = Tokenizer::Json(JsonField);
        tantivy_index
            .tokenizers()
            .register(tokenizer.as_str(), tokenizer);

        let reader: IndexReader = tantivy_index.reader_builder().try_into()?;

        let fastfield_reader = FastFieldReader::new(&reader.searcher());

        Ok(InvertedIndex {
            writer: None,
            reader,
            schema: Arc::new(schema),
            path: path.as_ref().to_str().unwrap().to_string(),
            tantivy_index,
            snippet_config: SnippetConfig::default(),
            fastfield_reader,
            rake: RakeModel::default(),
        })
    }

    pub fn fastfield_reader(&self) -> FastFieldReader {
        self.fastfield_reader.clone()
    }

    pub fn prepare_writer(&mut self) -> Result<()> {
        if self.writer.is_some() {
            return Ok(());
        }

        let writer = self
            .tantivy_index
            .writer_with_num_threads(1, 1_000_000_000)?;

        let merge_policy = NoMergePolicy;
        writer.set_merge_policy(Box::new(merge_policy));

        self.writer = Some(writer);

        Ok(())
    }

    pub fn set_snippet_config(&mut self, config: SnippetConfig) {
        self.snippet_config = config;
    }

    pub fn set_auto_merge_policy(&mut self) {
        let merge_policy = tantivy::merge_policy::LogMergePolicy::default();
        self.writer
            .as_mut()
            .expect("writer has not been prepared")
            .set_merge_policy(Box::new(merge_policy));
    }

    pub fn tokenizers(&self) -> &TokenizerManager {
        self.tantivy_index.tokenizers()
    }

    #[cfg(test)]
    pub fn temporary() -> Result<Self> {
        let path = crate::gen_temp_path();
        let mut s = Self::open(path)?;

        s.prepare_writer()?;

        Ok(s)
    }

    pub fn insert(&self, webpage: Webpage) -> Result<()> {
        self.writer
            .as_ref()
            .expect("writer has not been prepared")
            .add_document(webpage.into_tantivy(&self.schema, &self.rake)?)?;
        Ok(())
    }

    pub fn commit(&mut self) -> Result<()> {
        self.prepare_writer()?;
        self.writer
            .as_mut()
            .expect("writer has not been prepared")
            .commit()?;
        self.reader.reload()?;
        self.fastfield_reader = FastFieldReader::new(&self.reader.searcher());

        Ok(())
    }

    fn delete(&self, query: Box<dyn tantivy::query::Query>) -> Result<()> {
        self.writer
            .as_ref()
            .expect("writer has not been prepared")
            .delete_query(query)?;

        Ok(())
    }

    pub fn delete_all_before(&self, timestamp: tantivy::DateTime) -> Result<()> {
        let query = tantivy::query::RangeQuery::new_date_bounds(
            Field::Text(TextField::InsertionTimestamp)
                .name()
                .to_string(),
            std::ops::Bound::Unbounded,
            std::ops::Bound::Excluded(timestamp),
        );

        self.delete(Box::new(query))
    }

    pub fn search_initial(
        &self,
        query: &Query,
        ctx: &Ctx,
        collector: MainCollector,
    ) -> Result<InitialSearchResult> {
        if !query.count_results() {
            let mut query: Box<dyn tantivy::query::Query> = Box::new(query.clone());

            if let Some(limit) = collector.top_docs().max_docs() {
                let docs_per_segment = limit.total_docs / limit.segments;
                query = Box::new(ShortCircuitQuery::new(query, docs_per_segment as u64));
            }

            let pointers = ctx.tv_searcher.search(&query, &collector)?;

            return Ok(InitialSearchResult {
                num_websites: None,
                top_websites: pointers,
            });
        }

        let collector = (Count, collector);
        let (count, pointers) = ctx.tv_searcher.search(query, &collector)?;

        Ok(InitialSearchResult {
            num_websites: Some(count),
            top_websites: pointers,
        })
    }

    pub fn local_search_ctx(&self) -> Ctx {
        let tv_searcher = self.tv_searcher();
        Ctx {
            fastfield_reader: self.fastfield_reader.clone(),
            tv_searcher,
        }
    }

    pub fn tv_searcher(&self) -> tantivy::Searcher {
        self.reader.searcher()
    }

    pub fn retrieve_ranking_websites(
        &self,
        ctx: &Ctx,
        pointers: Vec<WebsitePointer>,
        mut aggregator: SignalAggregator,
        fastfield_reader: &FastFieldReader,
    ) -> Result<Vec<RankingWebsite>> {
        let mut top_websites = Vec::new();

        let mut pointers: Vec<_> = pointers.into_iter().enumerate().collect();
        pointers.sort_by(|a, b| {
            a.1.address
                .segment
                .cmp(&b.1.address.segment)
                .then_with(|| a.1.address.doc_id.cmp(&b.1.address.doc_id))
        });

        let mut prev_segment = None;
        for (orig_index, pointer) in pointers {
            let update_segment = match prev_segment {
                Some(prev_segment) if prev_segment != pointer.address.segment => true,
                None => true,
                _ => false,
            };

            if update_segment {
                let segment_reader = ctx.tv_searcher.segment_reader(pointer.address.segment);
                aggregator.register_segment(&ctx.tv_searcher, segment_reader, fastfield_reader)?;
            }

            prev_segment = Some(pointer.address.segment);

            top_websites.push((orig_index, RankingWebsite::new(pointer, &mut aggregator)));
        }

        top_websites.sort_by(|a, b| a.0.cmp(&b.0));

        Ok(top_websites
            .into_iter()
            .map(|(_, website)| website)
            .collect())
    }

    pub fn website_host_node(&self, website: &WebsitePointer) -> Result<Option<NodeID>> {
        let searcher = self.reader.searcher();
        let doc: TantivyDocument = searcher.doc(website.address.into())?;

        let field = self
            .schema()
            .get_field(Field::Fast(FastField::HostNodeID).name())
            .unwrap();

        let id = doc.get_first(field).unwrap().as_u64().unwrap();

        if id == u64::MAX {
            Ok(None)
        } else {
            Ok(Some(id.into()))
        }
    }

    pub fn retrieve_websites(
        &self,
        websites: &[WebsitePointer],
        query: &Query,
    ) -> Result<Vec<RetrievedWebpage>> {
        let tv_searcher = self.reader.searcher();
        let mut webpages: Vec<RetrievedWebpage> = websites
            .iter()
            .map(|website| self.retrieve_doc(website.address, &tv_searcher))
            .filter_map(|res| res.ok())
            .collect();

        for (url, page) in webpages.iter_mut().filter_map(|page| {
            let url = Url::parse(&page.url).ok()?;
            Some((url, page))
        }) {
            if query.simple_terms().is_empty() {
                let snippet = if let Some(description) = page.description.as_deref() {
                    let snip = description
                        .split_whitespace()
                        .take(self.snippet_config.empty_query_snippet_words)
                        .join(" ");

                    if snip.split_whitespace().count() < self.snippet_config.min_description_words {
                        page.body
                            .split_whitespace()
                            .take(self.snippet_config.empty_query_snippet_words)
                            .join(" ")
                    } else {
                        snip
                    }
                } else {
                    page.body
                        .split_whitespace()
                        .take(self.snippet_config.empty_query_snippet_words)
                        .join(" ")
                };

                page.snippet = TextSnippet {
                    fragments: vec![TextSnippetFragment::new_unhighlighted(snippet)],
                };
            } else {
                let min_body_len = if url.is_homepage() {
                    self.snippet_config.min_body_length_homepage
                } else {
                    self.snippet_config.min_body_length
                };

                if page.body.split_whitespace().count() < min_body_len
                    && page
                        .description
                        .as_deref()
                        .unwrap_or_default()
                        .split_whitespace()
                        .count()
                        >= self.snippet_config.min_description_words
                {
                    page.snippet = snippet::generate(
                        query,
                        page.description.as_deref().unwrap_or_default(),
                        &page.region,
                        self.snippet_config.clone(),
                    );
                } else {
                    page.snippet = snippet::generate(
                        query,
                        &page.body,
                        &page.region,
                        self.snippet_config.clone(),
                    );
                }
            }
        }

        Ok(webpages)
    }

    pub fn merge_into_max_segments(&mut self, max_num_segments: u64) -> Result<()> {
        self.prepare_writer()?;
        let base_path = Path::new(&self.path);
        let segments: Vec<_> = self
            .tantivy_index
            .load_metas()?
            .segments
            .into_iter()
            .collect();

        merge_tantivy_segments(
            self.writer.as_mut().expect("writer has not been prepared"),
            segments,
            base_path,
            max_num_segments,
        )?;

        Ok(())
    }

    fn retrieve_doc(
        &self,
        doc_address: DocAddress,
        searcher: &tantivy::Searcher,
    ) -> Result<RetrievedWebpage> {
        let doc: TantivyDocument = searcher.doc(doc_address.into())?;
        Ok(RetrievedWebpage::from(doc))
    }

    pub fn merge(mut self, mut other: InvertedIndex) -> Self {
        self.prepare_writer().expect("failed to prepare writer");
        other.prepare_writer().expect("failed to prepare writer");

        let path = self.path.clone();

        {
            other.commit().expect("failed to commit index");
            self.commit().expect("failed to commit index");

            let other_meta = other
                .tantivy_index
                .load_metas()
                .expect("failed to load tantivy metadata for index");

            let mut meta = self
                .tantivy_index
                .load_metas()
                .expect("failed to load tantivy metadata for index");

            let other_path = other.path.clone();
            let other_path = Path::new(other_path.as_str());
            other
                .writer
                .take()
                .expect("writer has not been prepared")
                .wait_merging_threads()
                .unwrap();

            let path = self.path.clone();
            let self_path = Path::new(path.as_str());
            self.writer
                .take()
                .expect("writer has not been prepared")
                .wait_merging_threads()
                .unwrap();

            let ids: HashSet<_> = meta.segments.iter().map(|segment| segment.id()).collect();

            for segment in other_meta.segments {
                if ids.contains(&segment.id()) {
                    continue;
                }

                // TODO: handle case where current index has segment with same name
                for file in segment.list_files() {
                    let p = other_path.join(&file);
                    if p.exists() {
                        fs::rename(p, self_path.join(&file)).unwrap();
                    }
                }
                meta.segments.push(segment);
            }

            meta.segments
                .sort_by_key(|a| std::cmp::Reverse(a.max_doc()));

            fs::remove_dir_all(other_path).ok();

            let self_path = Path::new(&path);

            std::fs::write(
                self_path.join("meta.json"),
                serde_json::to_string_pretty(&meta).unwrap(),
            )
            .unwrap();
        }

        let mut res = Self::open(path).expect("failed to open index");

        res.prepare_writer().expect("failed to prepare writer");

        res
    }

    pub fn stop(mut self) {
        self.writer
            .take()
            .expect("writer has not been prepared")
            .wait_merging_threads()
            .unwrap()
    }

    pub fn schema(&self) -> Arc<Schema> {
        Arc::clone(&self.schema)
    }

    pub fn num_segments(&self) -> usize {
        self.tantivy_index.searchable_segments().unwrap().len()
    }

    pub(crate) fn get_webpage(&self, url: &str) -> Option<RetrievedWebpage> {
        let url = Url::parse(url).ok()?;
        let tv_searcher = self.reader.searcher();
        let field = tv_searcher
            .schema()
            .get_field(Field::Text(TextField::UrlNoTokenizer).name())
            .unwrap();

        let term = tantivy::Term::from_field_text(field, url.as_str());

        let query = tantivy::query::TermQuery::new(term, tantivy::schema::IndexRecordOption::Basic);

        let mut res = tv_searcher
            .search(&query, &tantivy::collector::TopDocs::with_limit(1))
            .unwrap();

        res.pop()
            .map(|(_, doc)| self.retrieve_doc(doc.into(), &tv_searcher).unwrap())
    }

    pub(crate) fn get_homepage(&self, url: &Url) -> Option<RetrievedWebpage> {
        let tv_searcher = self.reader.searcher();
        let field = tv_searcher
            .schema()
            .get_field(Field::Text(TextField::SiteIfHomepageNoTokenizer).name())
            .unwrap();

        let host = url.normalized_host().unwrap_or_default();

        let term = tantivy::Term::from_field_text(field, host);

        let query = tantivy::query::TermQuery::new(term, tantivy::schema::IndexRecordOption::Basic);

        let mut res = tv_searcher
            .search(&query, &tantivy::collector::TopDocs::with_limit(1))
            .unwrap();

        res.pop()
            .map(|(_, doc)| self.retrieve_doc(doc.into(), &tv_searcher).unwrap())
    }
}

#[derive(Debug, Serialize)]
pub struct SearchResult {
    pub num_docs: Option<usize>,
    pub documents: Vec<RetrievedWebpage>,
}

#[derive(Default, Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RetrievedWebpage {
    pub title: String,
    pub url: String,
    pub body: String,
    pub snippet: TextSnippet,
    pub dirty_body: String,
    pub description: Option<String>,
    pub dmoz_description: Option<String>,
    pub updated_time: Option<NaiveDateTime>,
    pub schema_org: Vec<schema_org::Item>,
    pub region: Region,
    pub likely_has_ads: bool,
    pub likely_has_paywall: bool,
    pub recipe_first_ingredient_tag_id: Option<String>,
    pub keywords: Vec<String>,
}
impl RetrievedWebpage {
    pub fn description(&self) -> Option<&String> {
        self.description.as_ref().or(self.dmoz_description.as_ref())
    }
}

impl From<TantivyDocument> for RetrievedWebpage {
    fn from(doc: TantivyDocument) -> Self {
        let mut webpage = RetrievedWebpage::default();

        for value in doc.field_values() {
            match Field::get(value.field.field_id() as usize).copied() {
                Some(Field::Text(TextField::Title)) => {
                    webpage.title = value
                        .value()
                        .as_value()
                        .as_str()
                        .expect("Title field should be text")
                        .to_string();
                }
                Some(Field::Text(TextField::StemmedCleanBody)) => {
                    webpage.body = value
                        .value()
                        .as_value()
                        .as_str()
                        .expect("Body field should be text")
                        .to_string();
                }
                Some(Field::Text(TextField::Description)) => {
                    let desc = value
                        .value()
                        .as_value()
                        .as_str()
                        .expect("Description field should be text")
                        .to_string();

                    webpage.description = if desc.is_empty() { None } else { Some(desc) }
                }
                Some(Field::Text(TextField::Url)) => {
                    webpage.url = value
                        .value()
                        .as_value()
                        .as_str()
                        .expect("Url field should be text")
                        .to_string();
                }
                Some(Field::Fast(FastField::LastUpdated)) => {
                    webpage.updated_time = {
                        let timestamp = value.value().as_value().as_u64().unwrap() as i64;
                        if timestamp == 0 {
                            None
                        } else {
                            NaiveDateTime::from_timestamp_opt(timestamp, 0)
                        }
                    }
                }
                Some(Field::Text(TextField::AllBody)) => {
                    webpage.dirty_body = value
                        .value()
                        .as_value()
                        .as_str()
                        .expect("All body field should be text")
                        .to_string();
                }
                Some(Field::Fast(FastField::Region)) => {
                    webpage.region = {
                        let id = value.value().as_value().as_u64().unwrap();
                        Region::from_id(id)
                    }
                }
                Some(Field::Text(TextField::DmozDescription)) => {
                    let desc = value
                        .value()
                        .as_value()
                        .as_str()
                        .expect("Dmoz description field should be text")
                        .to_string();

                    webpage.dmoz_description = if desc.is_empty() { None } else { Some(desc) }
                }
                Some(Field::Text(TextField::SchemaOrgJson)) => {
                    let json = value
                        .value()
                        .as_value()
                        .as_str()
                        .expect("Schema.org json field should be stored as text")
                        .to_string();

                    webpage.schema_org = serde_json::from_str(&json).unwrap_or_default();
                }
                Some(Field::Fast(FastField::LikelyHasAds)) => {
                    webpage.likely_has_ads =
                        value.value().as_value().as_u64().unwrap_or_default() != 0;
                }
                Some(Field::Fast(FastField::LikelyHasPaywall)) => {
                    webpage.likely_has_paywall =
                        value.value().as_value().as_u64().unwrap_or_default() != 0;
                }
                Some(Field::Text(TextField::RecipeFirstIngredientTagId)) => {
                    let tag_id = value
                        .value()
                        .as_value()
                        .as_str()
                        .expect("Recipe first ingredient tag id field should be stored as text")
                        .to_string();

                    if !tag_id.is_empty() {
                        webpage.recipe_first_ingredient_tag_id = Some(tag_id);
                    }
                }
                Some(Field::Text(TextField::Keywords)) => {
                    let keywords = value
                        .value()
                        .as_value()
                        .as_str()
                        .expect("Keywords field should be stored as text")
                        .to_string();

                    webpage.keywords = keywords.split('\n').map(|s| s.to_string()).collect();
                }
                _ => {}
            }
        }

        webpage
    }
}

#[cfg(test)]
mod tests {
    use maplit::hashmap;

    use crate::{
        ranking::{Ranker, SignalAggregator},
        searcher::SearchQuery,
        webpage::Html,
    };

    use super::*;

    const CONTENT: &str = "this is the best example website ever this is the best example website ever this is the best example website ever this is the best example website ever this is the best example website ever this is the best example website ever";

    fn search(
        index: &InvertedIndex,
        query: &Query,
        ctx: &Ctx,
        collector: MainCollector,
    ) -> Result<SearchResult> {
        let initial_result = index.search_initial(query, ctx, collector)?;

        let pointers: Vec<_> = initial_result.top_websites;

        let websites = index.retrieve_websites(&pointers, query)?;

        Ok(SearchResult {
            num_docs: initial_result.num_websites,
            documents: websites,
        })
    }

    #[test]
    fn simple_search() {
        let mut index = InvertedIndex::temporary().expect("Unable to open index");
        let ctx = index.local_search_ctx();

        let query = Query::parse(
            &ctx,
            &SearchQuery {
                query: "test".to_string(),
                ..Default::default()
            },
            &index,
        )
        .expect("Failed to parse query");

        let ranker = Ranker::new(
            SignalAggregator::new(Some(&query)),
            ctx.fastfield_reader.clone(),
            Default::default(),
        );
        let result =
            search(&index, &query, &ctx, ranker.collector(ctx.clone())).expect("Search failed");
        assert_eq!(result.documents.len(), 0);

        index
            .insert(
                Webpage::new(
                    &format!(
                        r#"
                        <html>
                            <head>
                                <title>Test website</title>
                            </head>
                            <body>
                                {CONTENT}
                            </body>
                        </html>
                    "#
                    ),
                    "https://www.example.com",
                )
                .unwrap(),
            )
            .expect("failed to insert webpage");
        index.commit().expect("failed to commit index");
        let ctx = index.local_search_ctx();

        let ranker = Ranker::new(
            SignalAggregator::new(Some(&query)),
            ctx.fastfield_reader.clone(),
            Default::default(),
        );

        let result =
            search(&index, &query, &ctx, ranker.collector(ctx.clone())).expect("Search failed");
        assert_eq!(result.documents.len(), 1);
        assert_eq!(result.documents[0].url, "https://www.example.com/");
    }

    #[test]
    fn document_not_matching() {
        let mut index = InvertedIndex::temporary().expect("Unable to open index");

        index
            .insert(
                Webpage::new(
                    &format!(
                        r#"
                        <html>
                            <head>
                                <title>Test website</title>
                            </head>
                            <body>
                                {CONTENT}
                            </body>
                        </html>
                    "#
                    ),
                    "https://www.example.com",
                )
                .unwrap(),
            )
            .expect("failed to insert webpage");
        index.commit().expect("failed to commit index");

        let ctx = index.local_search_ctx();
        let query = Query::parse(
            &ctx,
            &SearchQuery {
                query: "this query should not match".to_string(),
                ..Default::default()
            },
            &index,
        )
        .expect("Failed to parse query");

        let ranker = Ranker::new(
            SignalAggregator::new(Some(&query)),
            ctx.fastfield_reader.clone(),
            Default::default(),
        );

        let result =
            search(&index, &query, &ctx, ranker.collector(ctx.clone())).expect("Search failed");
        assert_eq!(result.documents.len(), 0);
    }

    #[test]
    fn english_stemming() {
        let mut index = InvertedIndex::temporary().expect("Unable to open index");

        index
            .insert(
                Webpage::new(
                    &format!(
                        r#"
            <html>
                <head>
                    <title>Website for runners</title>
                </head>
                <body>
                    {CONTENT}
                </body>
            </html>
            "#
                    ),
                    "https://www.example.com",
                )
                .unwrap(),
            )
            .expect("failed to insert webpage");
        index.commit().expect("failed to commit index");

        let ctx = index.local_search_ctx();
        let query = Query::parse(
            &ctx,
            &SearchQuery {
                query: "runner".to_string(),
                ..Default::default()
            },
            &index,
        )
        .expect("Failed to parse query");
        let ranker = Ranker::new(
            SignalAggregator::new(Some(&query)),
            ctx.fastfield_reader.clone(),
            Default::default(),
        );

        let result =
            search(&index, &query, &ctx, ranker.collector(ctx.clone())).expect("Search failed");
        assert_eq!(result.documents.len(), 1);
        assert_eq!(result.documents[0].url, "https://www.example.com/");
    }

    #[test]
    fn stemmed_query_english() {
        let mut index = InvertedIndex::temporary().expect("Unable to open index");

        index
            .insert(
                Webpage::new(
                    &format!(
                        r#"
            <html>
                <head>
                    <title>Fast runner</title>
                </head>
                <body>
                    {CONTENT}
                </body>
            </html>
            "#
                    ),
                    "https://www.example.com",
                )
                .unwrap(),
            )
            .expect("failed to insert webpage");
        index.commit().expect("failed to commit index");

        let ctx = index.local_search_ctx();
        let query = Query::parse(
            &ctx,
            &SearchQuery {
                query: "runners".to_string(),
                ..Default::default()
            },
            &index,
        )
        .expect("Failed to parse query");
        let ranker = Ranker::new(
            SignalAggregator::new(Some(&query)),
            ctx.fastfield_reader.clone(),
            Default::default(),
        );

        let result =
            search(&index, &query, &ctx, ranker.collector(ctx.clone())).expect("Search failed");
        assert_eq!(result.documents.len(), 1);
        assert_eq!(result.documents[0].url, "https://www.example.com/");
    }

    #[test]
    fn not_searchable_backlinks() {
        let mut index = InvertedIndex::temporary().expect("Unable to open index");

        index
            .insert(
                Webpage::new(
                    &format!(
                        r#"
            <html>
                <head>
                    <title>Website A</title>
                </head>
                <a href="https://www.b.com">B site is great</a>
                {CONTENT}
            </html>
            "#
                    ),
                    "https://www.a.com",
                )
                .unwrap(),
            )
            .expect("failed to insert webpage");
        index
            .insert(Webpage {
                html: Html::parse(
                    &format!(
                        r#"
            <html>
                <head>
                    <title>Website B</title>
                </head>
                <body>
                    {CONTENT}
                </body>
            </html>
            "#
                    ),
                    "https://www.b.com",
                )
                .unwrap(),
                backlink_labels: vec!["B site is great".to_string()],
                host_centrality: 1.0,
                fetch_time_ms: 500,
                ..Default::default()
            })
            .expect("failed to insert webpage");

        index.commit().expect("failed to commit index");

        let ctx = index.local_search_ctx();
        let query = Query::parse(
            &ctx,
            &SearchQuery {
                query: "great site".to_string(),
                ..Default::default()
            },
            &index,
        )
        .expect("Failed to parse query");
        let ranker = Ranker::new(
            SignalAggregator::new(Some(&query)),
            ctx.fastfield_reader.clone(),
            Default::default(),
        );

        let mut result =
            search(&index, &query, &ctx, ranker.collector(ctx.clone())).expect("Search failed");

        result
            .documents
            .sort_by(|a, b| a.url.partial_cmp(&b.url).unwrap());

        assert_eq!(result.documents.len(), 1);
        assert_eq!(result.documents[0].url, "https://www.a.com/");
    }

    #[test]
    fn limited_top_docs() {
        let mut index = InvertedIndex::temporary().expect("Unable to open index");

        for _ in 0..100 {
            let dedup_s = crate::rand_words(100);

            index
                .insert(
                    Webpage::new(
                        &format!(
                            r#"
                    <html>
                        <head>
                            <title>Website for runners</title>
                        </head>
                        <body>
                            {CONTENT} {dedup_s}
                        </body>
                    </html>
                    "#
                        ),
                        "https://www.example.com",
                    )
                    .unwrap(),
                )
                .expect("failed to insert webpage");
        }

        index.commit().expect("failed to commit index");

        let ctx = index.local_search_ctx();
        let query = Query::parse(
            &ctx,
            &SearchQuery {
                query: "runner".to_string(),
                ..Default::default()
            },
            &index,
        )
        .expect("Failed to parse query");
        let ranker = Ranker::new(
            SignalAggregator::new(Some(&query)),
            ctx.fastfield_reader.clone(),
            Default::default(),
        );

        let result =
            search(&index, &query, &ctx, ranker.collector(ctx.clone())).expect("Search failed");
        assert_eq!(result.documents.len(), 20);
    }

    #[test]
    fn host_search() {
        let mut index = InvertedIndex::temporary().expect("Unable to open index");

        index
            .insert(
                Webpage::new(
                    &format!(
                        r#"
                    <html>
                        <head>
                            <title>News website</title>
                        </head>
                        <body>
                            {CONTENT}
                        </body>
                    </html>
                "#
                    ),
                    "https://www.dr.dk",
                )
                .unwrap(),
            )
            .expect("failed to insert webpage");
        index.commit().expect("failed to commit index");

        let ctx = index.local_search_ctx();
        let query = Query::parse(
            &ctx,
            &SearchQuery {
                query: "dr".to_string(),
                ..Default::default()
            },
            &index,
        )
        .expect("Failed to parse query");
        let ranker = Ranker::new(
            SignalAggregator::new(Some(&query)),
            ctx.fastfield_reader.clone(),
            Default::default(),
        );

        let result =
            search(&index, &query, &ctx, ranker.collector(ctx.clone())).expect("Search failed");
        assert_eq!(result.documents.len(), 1);
        assert_eq!(result.documents[0].url, "https://www.dr.dk/");
    }

    #[test]
    fn merge() {
        let index1 = InvertedIndex::temporary().expect("Unable to open index");

        index1
            .insert(
                Webpage::new(
                    &format!(
                        r#"
            <html>
                <head>
                    <title>Test website</title>
                </head>
                <body>
                    {CONTENT} {}
                </body>
            </html>
            "#,
                        crate::rand_words(100)
                    ),
                    "https://www.example.com",
                )
                .unwrap(),
            )
            .expect("failed to insert webpage");

        let index2 = InvertedIndex::temporary().expect("Unable to open index");

        index2
            .insert(
                Webpage::new(
                    &format!(
                        r#"
            <html>
                <head>
                    <title>Test website</title>
                </head>
                <body>
                    {CONTENT} {}
                </body>
            </html>
            "#,
                        crate::rand_words(100)
                    ),
                    "https://www.example.com",
                )
                .unwrap(),
            )
            .expect("failed to insert webpage");

        let mut index = index1.merge(index2);
        index.prepare_writer().unwrap();
        index.commit().unwrap();

        let ctx = index.local_search_ctx();
        let query = Query::parse(
            &ctx,
            &SearchQuery {
                query: "website".to_string(),
                ..Default::default()
            },
            &index,
        )
        .expect("Failed to parse query");
        let ranker = Ranker::new(
            SignalAggregator::new(Some(&query)),
            ctx.fastfield_reader.clone(),
            Default::default(),
        );

        let result =
            search(&index, &query, &ctx, ranker.collector(ctx.clone())).expect("Search failed");
        assert_eq!(result.documents.len(), 2);
        assert_eq!(result.documents[0].url, "https://www.example.com/");
        assert_eq!(result.documents[1].url, "https://www.example.com/");
    }

    #[test]
    fn match_across_fields() {
        let mut index = InvertedIndex::temporary().expect("Unable to open index");

        let ctx = index.local_search_ctx();
        let query = Query::parse(
            &ctx,
            &SearchQuery {
                query: "example test".to_string(),
                ..Default::default()
            },
            &index,
        )
        .expect("Failed to parse query");
        let ranker = Ranker::new(
            SignalAggregator::new(Some(&query)),
            ctx.fastfield_reader.clone(),
            Default::default(),
        );

        let result =
            search(&index, &query, &ctx, ranker.collector(ctx.clone())).expect("Search failed");
        assert_eq!(result.documents.len(), 0);

        index
            .insert(
                Webpage::new(
                    &format!(
                        r#"
                        <html>
                            <head>
                                <title>Test website</title>
                            </head>
                            <body>
                                {CONTENT}
                            </body>
                        </html>
                    "#
                    ),
                    "https://www.example.com",
                )
                .unwrap(),
            )
            .expect("failed to insert webpage");
        index.commit().expect("failed to commit index");

        let ctx = index.local_search_ctx();
        let ranker = Ranker::new(
            SignalAggregator::new(Some(&query)),
            ctx.fastfield_reader.clone(),
            Default::default(),
        );
        let result =
            search(&index, &query, &ctx, ranker.collector(ctx.clone())).expect("Search failed");
        assert_eq!(result.documents.len(), 1);
        assert_eq!(result.documents[0].url, "https://www.example.com/");
    }

    #[test]
    fn id_links_removed_during_indexing() {
        let mut index = InvertedIndex::temporary().expect("Unable to open index");

        index
            .insert(
                Webpage::new(
                    &format!(
                        r#"
                        <html>
                            <head>
                                <title>Test website</title>
                            </head>
                            <body>
                                {CONTENT}
                            </body>
                        </html>
                    "#
                    ),
                    "https://www.example.com#tag",
                )
                .unwrap(),
            )
            .expect("failed to insert webpage");
        index.commit().expect("failed to commit index");

        let ctx = index.local_search_ctx();
        let query = Query::parse(
            &ctx,
            &SearchQuery {
                query: "website".to_string(),
                ..Default::default()
            },
            &index,
        )
        .expect("Failed to parse query");
        let ranker = Ranker::new(
            SignalAggregator::new(Some(&query)),
            ctx.fastfield_reader.clone(),
            Default::default(),
        );

        let result =
            search(&index, &query, &ctx, ranker.collector(ctx.clone())).expect("Search failed");
        assert_eq!(result.documents.len(), 1);
        assert_eq!(result.documents[0].url, "https://www.example.com/");
    }

    #[test]
    fn schema_org_stored() {
        let mut index = InvertedIndex::temporary().expect("Unable to open index");

        index
            .insert(Webpage::new(
                &format!(
                    r#"
                    <html>
                        <head>
                            <title>News website</title>
                            <script type="application/ld+json">{{"@context":"http://schema.org","@type":"LiveBlogPosting","coverageStartTime":"2022-11-14T23:45:00+00:00","coverageEndTime":"2022-11-15T23:45:00.000Z","datePublished":"2022-11-14T23:45:00+00:00","articleBody":"","author":[{{"name":"DR"}}],"copyrightYear":2022}}</script>
                        </head>
                        <body>
                            {CONTENT} test
                            <article itemscope="" itemType="http://schema.org/NewsArticle">
                                <div itemProp="publisher" itemscope="" itemType="https://schema.org/Organization"><meta itemProp="name" content="DR"/>
                                </div>
                            </article>
                        </body>
                    </html>
                "#
                ),
                "https://www.example.com",
            ).unwrap())
            .expect("failed to insert webpage");

        index.commit().expect("failed to commit index");

        let ctx = index.local_search_ctx();
        let query = Query::parse(
            &ctx,
            &SearchQuery {
                query: "test".to_string(),
                ..Default::default()
            },
            &index,
        )
        .expect("Failed to parse query");
        let ranker = Ranker::new(
            SignalAggregator::new(Some(&query)),
            ctx.fastfield_reader.clone(),
            Default::default(),
        );

        let result =
            search(&index, &query, &ctx, ranker.collector(ctx.clone())).expect("Search failed");
        assert_eq!(result.documents.len(), 1);
        let schema = result.documents[0].schema_org.clone();

        assert_eq!(schema.len(), 2);

        assert_eq!(
            schema[0].itemtype,
            Some(schema_org::OneOrMany::One("LiveBlogPosting".to_string()))
        );
        assert_eq!(
            schema[0].properties.get("coverageStartTime"),
            Some(&schema_org::OneOrMany::One(schema_org::Property::String(
                "2022-11-14T23:45:00+00:00".to_string()
            )))
        );
        assert_eq!(
            schema[1].itemtype,
            Some(schema_org::OneOrMany::One("NewsArticle".to_string()))
        );
        assert_eq!(
            schema[1].properties.get("publisher"),
            Some(&schema_org::OneOrMany::One(schema_org::Property::Item(
                schema_org::Item {
                    itemtype: Some(schema_org::OneOrMany::One("Organization".to_string())),
                    properties: hashmap! {
                        "name".to_string() => schema_org::OneOrMany::One(schema_org::Property::String("DR".to_string()))
                    }
                }
            )))
        );
    }

    #[test]
    fn get_webpage() {
        let mut index = InvertedIndex::temporary().expect("Unable to open index");

        index
            .insert(Webpage::new(
                &format!(
                    r#"
                    <html>
                        <head>
                            <title>News website</title>
                            <script type="application/ld+json">{{"@context":"http://schema.org","@type":"LiveBlogPosting","coverageStartTime":"2022-11-14T23:45:00+00:00","coverageEndTime":"2022-11-15T23:45:00.000Z","datePublished":"2022-11-14T23:45:00+00:00","articleBody":"","author":[{{"name":"DR"}}],"copyrightYear":2022}}</script>
                        </head>
                        <body>
                            {CONTENT} test
                            <article itemscope="" itemType="http://schema.org/NewsArticle">
                                <div itemProp="publisher" itemscope="" itemType="https://schema.org/Organization"><meta itemProp="name" content="DR"/>
                                </div>
                            </article>
                        </body>
                    </html>
                "#
                ),
                "https://www.example.com",
            ).unwrap())
            .expect("failed to insert webpage");

        index.commit().expect("failed to commit index");

        let webpage = index.get_webpage("https://www.example.com").unwrap();
        assert_eq!(webpage.title, "News website".to_string());
        assert_eq!(webpage.url, "https://www.example.com/".to_string());
    }

    #[test]
    fn get_homepage() {
        let mut index = InvertedIndex::temporary().expect("Unable to open index");

        index
            .insert(Webpage::new(
                &format!(
                    r#"
                    <html>
                        <head>
                            <title>News website</title>
                            <script type="application/ld+json">{{"@context":"http://schema.org","@type":"LiveBlogPosting","coverageStartTime":"2022-11-14T23:45:00+00:00","coverageEndTime":"2022-11-15T23:45:00.000Z","datePublished":"2022-11-14T23:45:00+00:00","articleBody":"","author":[{{"name":"DR"}}],"url":"https://www.example.com","mainEntityOfPage":"https://www.example.com"}}
                            </script>
                        </head>
                        <body>
                            {CONTENT} test
                            <article itemscope="" itemType="http://schema.org/NewsArticle">
                                <div itemProp="publisher" itemscope="" itemType="https://schema.org/Organization"><meta itemProp="name" content="DR"/>
                                </div>
                            </article>
                        </body>
                    </html>
                "#
                ),
                "https://www.example.com",
            ).unwrap())
            .expect("failed to insert webpage");

        index.commit().expect("failed to commit index");

        let webpage = index
            .get_homepage(&Url::parse("https://www.example.com").unwrap())
            .unwrap();
        assert_eq!(webpage.title, "News website".to_string());
        assert_eq!(webpage.url, "https://www.example.com/".to_string());
    }

    #[test]
    fn insertion_time_deletion() {
        let mut index = InvertedIndex::temporary().expect("Unable to open index");

        let inserted_at = chrono::Utc::now() - chrono::Duration::days(4);
        let cutoff = inserted_at + chrono::Duration::hours(1);

        let a = Webpage {
            html: Html::parse(
                &format!(
                    r#"
                    <html>
                        <head>
                            <title>News website</title>
                        </head>
                        <body>
                            {CONTENT} test
                        </body>
                    </html>
                "#,
                    CONTENT = crate::rand_words(100)
                ),
                "https://www.a.com",
            )
            .unwrap(),
            inserted_at,
            ..Default::default()
        };
        let b = Webpage {
            html: Html::parse(
                &format!(
                    r#"
                    <html>
                        <head>
                            <title>News website</title>
                        </head>
                        <body>
                            {CONTENT} test
                        </body>
                    </html>
                "#,
                    CONTENT = crate::rand_words(100)
                ),
                "https://www.b.com",
            )
            .unwrap(),
            inserted_at: chrono::Utc::now(),
            ..Default::default()
        };

        index.insert(a).unwrap();
        index.insert(b).unwrap();

        index.commit().expect("failed to commit index");

        let ctx = index.local_search_ctx();
        let query = Query::parse(
            &ctx,
            &SearchQuery {
                query: "test".to_string(),
                ..Default::default()
            },
            &index,
        )
        .expect("Failed to parse query");

        let ranker = Ranker::new(
            SignalAggregator::new(Some(&query)),
            ctx.fastfield_reader.clone(),
            Default::default(),
        );

        let mut result =
            search(&index, &query, &ctx, ranker.collector(ctx.clone())).expect("Search failed");

        assert_eq!(result.documents.len(), 2);

        index
            .delete_all_before(tantivy::DateTime::from_utc(
                tantivy::time::OffsetDateTime::from_unix_timestamp(cutoff.timestamp()).unwrap(),
            ))
            .unwrap();
        index.commit().expect("failed to commit index");

        let ctx = index.local_search_ctx();

        let ranker = Ranker::new(
            SignalAggregator::new(Some(&query)),
            ctx.fastfield_reader.clone(),
            Default::default(),
        );

        result =
            search(&index, &query, &ctx, ranker.collector(ctx.clone())).expect("Search failed");

        assert_eq!(result.documents.len(), 1);
    }
}
