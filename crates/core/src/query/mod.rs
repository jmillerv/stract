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

use crate::{
    inverted_index::InvertedIndex,
    query::parser::TermCompound,
    ranking::SignalCoefficient,
    schema::{Field, TextField},
    search_ctx::Ctx,
    searcher::SearchQuery,
    webpage::{region::Region, safety_classifier},
    Result,
};
use optics::{HostRankings, Optic};
use std::collections::HashMap;
use tantivy::query::{BooleanQuery, Occur, QueryClone, TermQuery};

mod const_query;
pub mod intersection;
pub mod optic;
pub mod parser;
mod pattern_query;
pub mod shortcircuit;
pub mod union;

use parser::Term;

use self::{optic::AsMultipleTantivyQuery, parser::CompoundAwareTerm};

const MAX_SIMILAR_TERMS: usize = 10;

#[derive(Clone, Debug)]
pub struct Query {
    #[allow(clippy::vec_box)]
    terms: Vec<Box<Term>>,
    simple_terms_text: Vec<String>,
    tantivy_query: Box<BooleanQuery>,
    host_rankings: HostRankings,
    offset: usize,
    region: Option<Region>,
    optics: Vec<Optic>,
    top_n: usize,
    count_results: bool,
}

impl Query {
    pub fn parse(ctx: &Ctx, query: &SearchQuery, index: &InvertedIndex) -> Result<Query> {
        let parsed_terms = parser::parse(&query.query);
        let mut term_count = HashMap::new();
        let mut terms = Vec::new();

        for term in parsed_terms {
            let count = term_count.entry(term.clone()).or_insert(0);

            if *count < MAX_SIMILAR_TERMS {
                terms.push(term);
            }

            *count += 1;
        }

        let mut compound_terms: Vec<_> = terms
            .clone()
            .into_iter()
            .map(|term| CompoundAwareTerm {
                term: *term,
                adjacent_terms: Vec::new(),
            })
            .collect();

        let term_ids: Vec<_> = compound_terms.iter().enumerate().map(|(i, _)| i).collect();

        for window_size in 2..=3 {
            for window in term_ids.windows(window_size) {
                let mut window_terms = Vec::new();
                for i in window {
                    if let Term::Simple(t) = &compound_terms[*i].term {
                        window_terms.push(t.clone());
                    }
                }

                let compound = TermCompound {
                    terms: window_terms,
                };

                for i in window {
                    compound_terms[*i].adjacent_terms.push(compound.clone());
                }
            }
        }

        let schema = index.schema();

        let fields: Vec<tantivy::schema::Field> = schema.fields().map(|(field, _)| field).collect();

        let mut queries: Vec<(Occur, Box<dyn tantivy::query::Query + 'static>)> = compound_terms
            .iter()
            .map(|term| term.as_tantivy_query(&fields))
            .collect();

        if query.safe_search {
            let field = Field::Text(TextField::SafetyClassification);
            let field = schema.get_field(field.name()).unwrap();

            queries.push((
                Occur::MustNot,
                Box::new(TermQuery::new(
                    tantivy::Term::from_field_text(
                        field,
                        safety_classifier::Label::NSFW.to_string().as_str(),
                    ),
                    tantivy::schema::IndexRecordOption::Basic,
                )),
            ));
        }

        let mut tantivy_query = Box::new(BooleanQuery::new(queries));

        let simple_terms_text: Vec<String> = terms
            .clone()
            .into_iter()
            .filter_map(|term| term.as_simple_text().map(|s| s.to_string()))
            .flat_map(|term| {
                // term might be a phrase, so we split it into words
                term.split_ascii_whitespace()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
            })
            .collect();

        let mut optics = Vec::new();
        if let Some(site_rankigns_optic) = query.host_rankings.clone().map(|sr| sr.into_optic()) {
            optics.push(site_rankigns_optic);
        }

        if let Some(optic) = &query.optic {
            optics.push(optic.clone());
        }

        for optic in &optics {
            let mut subqueries = vec![(Occur::Must, tantivy_query.box_clone())];
            subqueries.append(&mut optic.as_multiple_tantivy(&schema, &ctx.fastfield_reader));
            tantivy_query = Box::new(BooleanQuery::new(subqueries));
        }

        Ok(Query {
            terms,
            host_rankings: optics.iter().fold(HostRankings::default(), |mut acc, el| {
                acc.merge_into(el.host_rankings.clone());
                acc
            }),
            simple_terms_text,
            tantivy_query,
            optics,
            offset: query.num_results * query.page,
            region: query.selected_region,
            top_n: query.num_results,
            count_results: query.count_results,
        })
    }

    pub fn count_results(&self) -> bool {
        self.count_results
    }

    pub fn simple_terms(&self) -> &[String] {
        &self.simple_terms_text
    }

    pub fn terms(&self) -> &[Box<Term>] {
        &self.terms
    }

    pub fn optics(&self) -> &[Optic] {
        &self.optics
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    pub fn num_results(&self) -> usize {
        self.top_n
    }

    pub fn offset(&self) -> usize {
        self.offset
    }

    pub fn region(&self) -> Option<&Region> {
        self.region.as_ref()
    }

    pub fn host_rankings(&self) -> &HostRankings {
        &self.host_rankings
    }

    pub fn signal_coefficients(&self) -> Option<SignalCoefficient> {
        if self.optics.is_empty() {
            return None;
        }

        Some(
            self.optics
                .iter()
                .fold(SignalCoefficient::default(), |mut acc, optic| {
                    let coeffs = SignalCoefficient::from_optic(optic);
                    acc.merge_into(coeffs);
                    acc
                }),
        )
    }
}

impl tantivy::query::Query for Query {
    fn weight(
        &self,
        enable_scoring: tantivy::query::EnableScoring,
    ) -> tantivy::Result<Box<dyn tantivy::query::Weight>> {
        self.tantivy_query.weight(enable_scoring)
    }

    fn query_terms<'a>(&'a self, visitor: &mut dyn FnMut(&'a tantivy::Term, bool)) {
        self.tantivy_query.query_terms(visitor)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        index::Index,
        rand_words,
        searcher::{LocalSearcher, SearchQuery},
        webpage::Webpage,
    };

    use super::*;

    fn empty_index() -> InvertedIndex {
        InvertedIndex::temporary().unwrap()
    }

    #[test]
    fn simple_parse() {
        let index = empty_index();
        let ctx = index.local_search_ctx();

        let query = Query::parse(
            &ctx,
            &SearchQuery {
                query: "this is a simple query the the the the the the the the the the the the the"
                    .to_string(),
                ..Default::default()
            },
            &index,
        )
        .expect("Failed to parse query");

        assert_eq!(
            query.simple_terms(),
            vec![
                "this".to_string(),
                "is".to_string(),
                "a".to_string(),
                "simple".to_string(),
                "query".to_string(),
                "the".to_string(),
                "the".to_string(),
                "the".to_string(),
                "the".to_string(),
                "the".to_string(),
                "the".to_string(),
                "the".to_string(),
                "the".to_string(),
                "the".to_string(),
                "the".to_string(),
            ]
        );
    }

    #[test]
    fn parse_trailing_leading_whitespace() {
        let index = empty_index();
        let ctx = index.local_search_ctx();

        let query = Query::parse(
            &ctx,
            &SearchQuery {
                query: "   this is a simple query   ".to_string(),
                ..Default::default()
            },
            &index,
        )
        .expect("Failed to parse query");

        assert_eq!(
            query.simple_terms(),
            vec![
                "this".to_string(),
                "is".to_string(),
                "a".to_string(),
                "simple".to_string(),
                "query".to_string(),
            ]
        );
    }

    #[test]
    fn parse_weird_characters() {
        let index = empty_index();
        let ctx = index.local_search_ctx();

        let terms = Query::parse(
            &ctx,
            &SearchQuery {
                query: "123".to_string(),
                ..Default::default()
            },
            &index,
        )
        .expect("Failed to parse query")
        .simple_terms()
        .to_vec();
        assert_eq!(terms, vec!["123".to_string()]);

        let terms = Query::parse(
            &ctx,
            &SearchQuery {
                query: "123 33".to_string(),
                ..Default::default()
            },
            &index,
        )
        .expect("Failed to parse query")
        .simple_terms()
        .to_vec();
        assert_eq!(terms, vec!["123".to_string(), "33".to_string()]);

        let terms = Query::parse(
            &ctx,
            &SearchQuery {
                query: "term! term# $".to_string(),
                ..Default::default()
            },
            &index,
        )
        .expect("Failed to parse query")
        .simple_terms()
        .to_vec();
        assert_eq!(
            terms,
            vec!["term!".to_string(), "term#".to_string(), "$".to_string()]
        );
    }

    #[test]
    fn simple_terms_phrase() {
        let index = empty_index();
        let ctx = index.local_search_ctx();

        let terms = Query::parse(
            &ctx,
            &SearchQuery {
                query: "\"test term\"".to_string(),
                ..Default::default()
            },
            &index,
        )
        .expect("Failed to parse query")
        .simple_terms()
        .to_vec();

        assert_eq!(terms, vec!["test".to_string(), "term".to_string()]);
    }

    #[test]
    fn not_query() {
        let mut index = Index::temporary().expect("Unable to open index");
        let query = SearchQuery {
            query: "test -website".to_string(),
            ..Default::default()
        };

        index
            .insert(
                Webpage::new(
                    r#"
                        <html>
                            <head>
                                <title>Test website</title>
                            </head>
                            <body>
                                This is a test website
                            </body>
                        </html>
                    "#,
                    "https://www.first.com",
                )
                .unwrap(),
            )
            .expect("failed to insert webpage");
        index
            .insert(
                Webpage::new(
                    r#"
                        <html>
                            <head>
                                <title>Test test</title>
                            </head>
                            <body>
                                This test page does not contain the forbidden word
                            </body>
                        </html>
                    "#,
                    "https://www.second.com",
                )
                .unwrap(),
            )
            .expect("failed to insert webpage");
        index.commit().expect("failed to commit index");
        let searcher = LocalSearcher::from(index);

        let result = searcher.search(&query).expect("Search failed");
        assert_eq!(result.webpages.len(), 1);
        assert_eq!(result.webpages[0].url, "https://www.second.com/");
    }

    #[test]
    fn site_query() {
        let mut index = Index::temporary().expect("Unable to open index");

        index
            .insert(
                Webpage::new(
                    r#"
                        <html>
                            <head>
                                <title>Test website</title>
                            </head>
                            <body>
                                This is a test website
                            </body>
                        </html>
                    "#,
                    "https://www.first.com",
                )
                .unwrap(),
            )
            .expect("failed to insert webpage");
        index
            .insert(
                Webpage::new(
                    r#"
                        <html>
                            <head>
                                <title>Test test</title>
                            </head>
                            <body>
                                This test page does not contain the forbidden word
                            </body>
                        </html>
                    "#,
                    "https://www.second.com",
                )
                .unwrap(),
            )
            .expect("failed to insert webpage");
        index.commit().expect("failed to commit index");
        let searcher = LocalSearcher::from(index);

        let query = SearchQuery {
            query: "test site:first.com".to_string(),
            ..Default::default()
        };
        let result = searcher.search(&query).expect("Search failed");
        assert_eq!(result.webpages.len(), 1);
        assert_eq!(result.webpages[0].url, "https://www.first.com/");

        let query = SearchQuery {
            query: "test site:www.first.com".to_string(),
            ..Default::default()
        };
        let result = searcher.search(&query).expect("Search failed");
        assert_eq!(result.webpages.len(), 1);
        assert_eq!(result.webpages[0].url, "https://www.first.com/");

        let query = SearchQuery {
            query: "test -site:first.com".to_string(),
            ..Default::default()
        };
        let result = searcher.search(&query).expect("Search failed");
        assert_eq!(result.webpages.len(), 1);
        assert_eq!(result.webpages[0].url, "https://www.second.com/");
    }

    #[test]
    fn title_query() {
        let mut index = Index::temporary().expect("Unable to open index");

        index
            .insert(
                Webpage::new(
                    r#"
                        <html>
                            <head>
                                <title>Test website</title>
                            </head>
                            <body>
                                This is a test website
                            </body>
                        </html>
                    "#,
                    "https://www.first.com",
                )
                .unwrap(),
            )
            .expect("failed to insert webpage");
        index
            .insert(
                Webpage::new(
                    r#"
                        <html>
                            <head>
                                <title>Test test</title>
                            </head>
                            <body>
                                This is a test website
                            </body>
                        </html>
                    "#,
                    "https://www.second.com",
                )
                .unwrap(),
            )
            .expect("failed to insert webpage");
        index.commit().expect("failed to commit index");
        let searcher = LocalSearcher::from(index);

        let query = SearchQuery {
            query: "intitle:website".to_string(),
            ..Default::default()
        };
        let result = searcher.search(&query).expect("Search failed");
        assert_eq!(result.webpages.len(), 1);
        assert_eq!(result.webpages[0].url, "https://www.first.com/");
    }

    #[test]
    fn url_query() {
        let mut index = Index::temporary().expect("Unable to open index");

        index
            .insert(
                Webpage::new(
                    r#"
                        <html>
                            <head>
                                <title>Test website</title>
                            </head>
                            <body>
                                This is a test website
                            </body>
                        </html>
                    "#,
                    "https://www.first.com/forum",
                )
                .unwrap(),
            )
            .expect("failed to insert webpage");
        index
            .insert(
                Webpage::new(
                    r#"
                        <html>
                            <head>
                                <title>Test test</title>
                            </head>
                            <body>
                                This is a test website
                            </body>
                        </html>
                    "#,
                    "https://www.second.com",
                )
                .unwrap(),
            )
            .expect("failed to insert webpage");
        index.commit().expect("failed to commit index");
        let searcher = LocalSearcher::from(index);

        let query = SearchQuery {
            query: "test inurl:forum".to_string(),
            ..Default::default()
        };
        let result = searcher.search(&query).expect("Search failed");
        assert_eq!(result.webpages.len(), 1);
        assert_eq!(result.webpages[0].url, "https://www.first.com/forum");
    }

    #[test]
    fn empty_query() {
        let index = empty_index();
        let ctx = index.local_search_ctx();

        let query = Query::parse(
            &ctx,
            &SearchQuery {
                query: "".to_string(),
                ..Default::default()
            },
            &index,
        )
        .expect("failed to parse query");

        assert!(query.is_empty())
    }

    #[test]
    fn query_term_only_special_char() {
        let index = empty_index();
        let ctx = index.local_search_ctx();

        let query = Query::parse(
            &ctx,
            &SearchQuery {
                query: "&".to_string(),
                ..Default::default()
            },
            &index,
        )
        .expect("Failed to parse query");

        assert!(!query.is_empty());
    }

    #[test]
    fn site_query_split_domain() {
        let mut index = Index::temporary().expect("Unable to open index");

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
                                This is a test website {}
                            </body>
                        </html>
                    "#,
                        rand_words(1000)
                    ),
                    "https://www.the-first.com",
                )
                .unwrap(),
            )
            .expect("failed to insert webpage");
        index
            .insert(
                Webpage::new(
                    &format!(
                        r#"
                        <html>
                            <head>
                                <title>Test test</title>
                            </head>
                            <body>
                                This test page does not contain the forbidden word {}
                            </body>
                        </html>
                    "#,
                        rand_words(1000)
                    ),
                    "https://www.second.com",
                )
                .unwrap(),
            )
            .expect("failed to insert webpage");
        index.commit().expect("failed to commit index");
        let searcher = LocalSearcher::from(index);

        let query = SearchQuery {
            query: "test site:first.com".to_string(),
            ..Default::default()
        };
        let result = searcher.search(&query).expect("Search failed");
        assert_eq!(result.webpages.len(), 0);

        let query = SearchQuery {
            query: "test site:the-first.com".to_string(),
            ..Default::default()
        };
        let result = searcher.search(&query).expect("Search failed");
        assert_eq!(result.webpages.len(), 1);
        assert_eq!(result.webpages[0].url, "https://www.the-first.com/");

        let query = SearchQuery {
            query: "test site:www.the-first.com".to_string(),
            ..Default::default()
        };
        let result = searcher.search(&query).expect("Search failed");
        assert_eq!(result.webpages.len(), 1);
        assert_eq!(result.webpages[0].url, "https://www.the-first.com/");
    }

    #[test]
    fn phrase_query() {
        let mut index = Index::temporary().expect("Unable to open index");

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
                                This is a test website {}
                            </body>
                        </html>
                    "#,
                        rand_words(1000)
                    ),
                    "https://www.first.com",
                )
                .unwrap(),
            )
            .expect("failed to insert webpage");
        index
            .insert(
                Webpage::new(
                    &format!(
                        r#"
                        <html>
                            <head>
                                <title>Test test</title>
                            </head>
                            <body>
                                This is a bad test website {}
                            </body>
                        </html>
                    "#,
                        rand_words(1000)
                    ),
                    "https://www.second.com",
                )
                .unwrap(),
            )
            .expect("failed to insert webpage");
        index.commit().expect("failed to commit index");
        let searcher = LocalSearcher::from(index);

        let query = SearchQuery {
            query: "\"Test website\"".to_string(),
            ..Default::default()
        };
        let result = searcher.search(&query).expect("Search failed");
        assert_eq!(result.webpages.len(), 1);
        assert_eq!(result.webpages[0].url, "https://www.first.com/");

        let query = SearchQuery {
            query: "\"Test website\" site:www.second.com".to_string(),
            ..Default::default()
        };
        let result = searcher.search(&query).expect("Search failed");
        assert_eq!(result.webpages.len(), 0);
    }

    #[test]
    fn match_compound_words() {
        let mut index = Index::temporary().expect("Unable to open index");

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
                                This is a test website {}
                            </body>
                        </html>
                    "#,
                        rand_words(1000)
                    ),
                    "https://www.first.com",
                )
                .unwrap(),
            )
            .expect("failed to insert webpage");

        index
            .insert(
                Webpage::new(
                    &format!(
                        r#"
                        <html>
                            <head>
                                <title>Testwebsite</title>
                            </head>
                            <body>
                                This is a testwebsite {}
                            </body>
                        </html>
                    "#,
                        rand_words(1000)
                    ),
                    "https://www.second.com",
                )
                .unwrap(),
            )
            .expect("failed to insert webpage");

        index.commit().expect("failed to commit index");
        let searcher = LocalSearcher::from(index);

        let query = SearchQuery {
            query: "testwebsite".to_string(),
            ..Default::default()
        };

        let result = searcher.search(&query).expect("Search failed");
        assert_eq!(result.webpages.len(), 2);

        let query = SearchQuery {
            query: "test website".to_string(),
            ..Default::default()
        };

        let result = searcher.search(&query).expect("Search failed");
        assert_eq!(result.webpages.len(), 2);
    }

    #[test]
    fn safe_search() {
        let mut index = Index::temporary().expect("Unable to open index");
        let mut webpage = Webpage::new(
            &format!(
                r#"
                <html>
                    <head>
                        <title>Test website</title>
                    </head>
                    <body>
                        This is a test website {}
                    </body>
                </html>
            "#,
                rand_words(1000)
            ),
            "https://www.sfw.com",
        )
        .unwrap();

        webpage.safety_classification = Some(safety_classifier::Label::SFW);
        webpage.html.set_clean_text("sfw".to_string());

        index.insert(webpage).expect("failed to insert webpage");

        let mut webpage = Webpage::new(
            &format!(
                r#"
                <html>
                    <head>
                        <title>Test website</title>
                    </head>
                    <body>
                        This is a test website {}
                    </body>
                </html>
            "#,
                rand_words(1000)
            ),
            "https://www.nsfw.com",
        )
        .unwrap();

        webpage.safety_classification = Some(safety_classifier::Label::NSFW);
        webpage.html.set_clean_text("nsfw".to_string());

        index.insert(webpage).expect("failed to insert webpage");

        index.commit().expect("failed to commit index");
        let searcher = LocalSearcher::from(index);

        let query = SearchQuery {
            query: "test".to_string(),
            safe_search: false,
            ..Default::default()
        };

        let result = searcher.search(&query).expect("Search failed");
        assert_eq!(result.webpages.len(), 2);

        let query = SearchQuery {
            query: "test".to_string(),
            safe_search: true,
            ..Default::default()
        };

        let result = searcher.search(&query).expect("Search failed");
        assert_eq!(result.webpages.len(), 1);

        assert_eq!(result.webpages[0].url, "https://www.sfw.com/");
    }

    #[test]
    fn suffix_domain_prefix_path_site_operator() {
        let mut index = Index::temporary().expect("Unable to open index");

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
                                This is a test website {}
                            </body>
                        </html>
                    "#,
                        rand_words(1000)
                    ),
                    "https://www.first.com/example",
                )
                .unwrap(),
            )
            .expect("failed to insert webpage");
        index
            .insert(
                Webpage::new(
                    &format!(
                        r#"
                        <html>
                            <head>
                                <title>Test test</title>
                            </head>
                            <body>
                                This is a test website {}
                            </body>
                        </html>
                    "#,
                        rand_words(1000)
                    ),
                    "https://www.second.com",
                )
                .unwrap(),
            )
            .expect("failed to insert webpage");
        index
            .insert(
                Webpage::new(
                    &format!(
                        r#"
                        <html>
                            <head>
                                <title>Test test</title>
                            </head>
                            <body>
                                This is a test website {}
                            </body>
                        </html>
                    "#,
                        rand_words(1000)
                    ),
                    "https://www.third.io",
                )
                .unwrap(),
            )
            .expect("failed to insert webpage");
        index.commit().expect("failed to commit index");
        let searcher = LocalSearcher::from(index);

        let query = SearchQuery {
            query: "test site:.com".to_string(),
            ..Default::default()
        };
        let result = searcher.search(&query).expect("Search failed");
        assert_eq!(result.webpages.len(), 2);

        let query = SearchQuery {
            query: "test site:.com/example".to_string(),
            ..Default::default()
        };
        let result = searcher.search(&query).expect("Search failed");
        assert_eq!(result.webpages.len(), 1);

        let query = SearchQuery {
            query: "test site:first.com/example".to_string(),
            ..Default::default()
        };
        let result = searcher.search(&query).expect("Search failed");
        assert_eq!(result.webpages.len(), 1);

        let query = SearchQuery {
            query: "test site:first.com".to_string(),
            ..Default::default()
        };
        let result = searcher.search(&query).expect("Search failed");
        assert_eq!(result.webpages.len(), 1);

        let query = SearchQuery {
            query: "test site:www.first.com".to_string(),
            ..Default::default()
        };
        let result = searcher.search(&query).expect("Search failed");
        assert_eq!(result.webpages.len(), 1);
    }
}
