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

use itertools::Itertools;
use optics::{Action, MatchLocation, Matching, Optic, Rule};
use tantivy::{
    query::{BooleanQuery, Occur, QueryClone},
    schema::Schema,
};

use crate::{fastfield_reader::FastFieldReader, schema::TextField};

use super::{const_query::ConstQuery, pattern_query::PatternQuery, union::UnionQuery};

pub trait AsTantivyQuery {
    fn as_tantivy(
        &self,
        schema: &Schema,
        fastfield_reader: &FastFieldReader,
    ) -> Box<dyn tantivy::query::Query>;
}

pub trait AsMultipleTantivyQuery {
    fn as_multiple_tantivy(
        &self,
        schema: &Schema,
        fastfield_reader: &FastFieldReader,
    ) -> Vec<(Occur, Box<dyn tantivy::query::Query>)>;
}

impl AsMultipleTantivyQuery for Optic {
    fn as_multiple_tantivy(
        &self,
        schema: &Schema,
        fastfields: &FastFieldReader,
    ) -> Vec<(Occur, Box<dyn tantivy::query::Query>)> {
        if self.discard_non_matching {
            vec![(
                Occur::Must,
                UnionQuery::from(
                    self.rules
                        .iter()
                        .chain(self.site_rankings.rules().iter())
                        .filter_map(|rule| rule.as_searchable_rule(schema, fastfields))
                        .map(|(occur, rule)| {
                            BooleanQuery::from(vec![(occur, rule.query)]).box_clone()
                        })
                        .collect_vec(),
                )
                .box_clone(),
            )]
        } else {
            self.rules
                .iter()
                .chain(self.site_rankings.rules().iter())
                .filter_map(|rule| rule.as_searchable_rule(schema, fastfields))
                .map(|(occur, rule)| (occur, rule.query))
                .collect()
        }
    }
}

pub struct SearchableRule {
    pub query: Box<dyn tantivy::query::Query>,
    pub boost: f64,
}

pub trait AsSearchableRule {
    fn as_searchable_rule(
        &self,
        schema: &Schema,
        fastfield_reader: &FastFieldReader,
    ) -> Option<(Occur, SearchableRule)>;
}

impl AsSearchableRule for Rule {
    fn as_searchable_rule(
        &self,
        schema: &Schema,
        fastfield_reader: &FastFieldReader,
    ) -> Option<(Occur, SearchableRule)> {
        let mut subqueries: Vec<_> = self
            .matches
            .iter()
            .map(|matching| (Occur::Must, matching.as_tantivy(schema, fastfield_reader)))
            .collect();

        if subqueries.is_empty() {
            return None;
        }

        let subquery = if subqueries.len() == 1 {
            subqueries.pop().unwrap().1
        } else {
            BooleanQuery::from(subqueries).box_clone()
        };

        match &self.action {
            Action::Boost(boost) => Some((
                Occur::Should,
                SearchableRule {
                    query: ConstQuery::new(subquery, 1.0).box_clone(),
                    boost: *boost as f64,
                },
            )),
            Action::Downrank(boost) => Some((
                Occur::Should,
                SearchableRule {
                    query: ConstQuery::new(subquery, 1.0).box_clone(),
                    boost: *boost as f64 * -1.0,
                },
            )),
            Action::Discard => Some((
                Occur::MustNot,
                SearchableRule {
                    query: subquery,
                    boost: 0.0,
                },
            )),
        }
    }
}

impl AsTantivyQuery for Matching {
    fn as_tantivy(
        &self,
        schema: &Schema,
        fastfield_reader: &FastFieldReader,
    ) -> Box<dyn tantivy::query::Query> {
        match &self.location {
            MatchLocation::Site => PatternQuery::new(
                self.pattern.clone(),
                TextField::Site,
                schema,
                fastfield_reader.clone(),
            )
            .box_clone(),
            MatchLocation::Url => PatternQuery::new(
                self.pattern.clone(),
                TextField::Url,
                schema,
                fastfield_reader.clone(),
            )
            .box_clone(),
            MatchLocation::Domain => PatternQuery::new(
                self.pattern.clone(),
                TextField::Domain,
                schema,
                fastfield_reader.clone(),
            )
            .box_clone(),
            MatchLocation::Title => PatternQuery::new(
                self.pattern.clone(),
                TextField::Title,
                schema,
                fastfield_reader.clone(),
            )
            .box_clone(),
            MatchLocation::Description => UnionQuery::from(vec![
                PatternQuery::new(
                    self.pattern.clone(),
                    TextField::Description,
                    schema,
                    fastfield_reader.clone(),
                )
                .box_clone(),
                PatternQuery::new(
                    self.pattern.clone(),
                    TextField::DmozDescription,
                    schema,
                    fastfield_reader.clone(),
                )
                .box_clone(),
            ])
            .box_clone(),
            MatchLocation::Content => PatternQuery::new(
                self.pattern.clone(),
                TextField::CleanBody,
                schema,
                fastfield_reader.clone(),
            )
            .box_clone(),
            MatchLocation::Schema => PatternQuery::new(
                self.pattern.clone(),
                TextField::FlattenedSchemaOrgJson,
                schema,
                fastfield_reader.clone(),
            )
            .box_clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use optics::SiteRankings;

    use crate::{
        gen_temp_path,
        index::Index,
        ranking::centrality_store::CentralityStore,
        searcher::{LocalSearcher, SearchQuery},
        webgraph::{Node, WebgraphBuilder},
        webpage::{Html, Webpage},
    };

    const CONTENT: &str = "this is the best example website ever this is the best example website ever this is the best example website ever this is the best example website ever this is the best example website ever this is the best example website ever";

    #[test]
    fn discard_and_boost_sites() {
        let mut index = Index::temporary().expect("Unable to open index");

        index
            .insert(Webpage {
                html: Html::parse(
                    &format!(
                        r#"
                    <html>
                        <head>
                            <title>Website A</title>
                        </head>
                        <body>
                            {CONTENT} {}
                            example example example
                        </body>
                    </html>
                "#,
                        crate::rand_words(100)
                    ),
                    "https://www.a.com",
                ),
                backlinks: vec![],
                host_centrality: 0.0,
                page_centrality: 0.0,
                fetch_time_ms: 500,
                pre_computed_score: 0.0,
                crawl_stability: 0.0,
                primary_image: None,
                node_id: None,
                host_topic: None,
                dmoz_description: None,
            })
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
                            {CONTENT} {}
                        </body>
                    </html>
                "#,
                        crate::rand_words(100)
                    ),
                    "https://www.b.com",
                ),
                backlinks: vec![],
                host_centrality: 0.01,
                page_centrality: 0.0,
                primary_image: None,
                pre_computed_score: 0.0,
                crawl_stability: 0.0,
                fetch_time_ms: 500,
                node_id: None,
                host_topic: None,
                dmoz_description: None,
            })
            .expect("failed to insert webpage");

        index.commit().expect("failed to commit index");
        let searcher = LocalSearcher::from(index);

        let res = searcher
            .search(&SearchQuery {
                query: "website".to_string(),
                ..Default::default()
            })
            .unwrap()
            .webpages;

        assert_eq!(res.len(), 2);
        assert_eq!(res[0].url, "https://www.b.com");
        assert_eq!(res[1].url, "https://www.a.com");

        let res = searcher
            .search(&SearchQuery {
                query: "website".to_string(),
                optic_program: Some(
                    r#"
                        Rule {
                            Matches {
                                Domain("b.com")
                            },
                            Action(Discard)
                        }
                    "#
                    .to_string(),
                ),
                ..Default::default()
            })
            .unwrap()
            .webpages;

        assert_eq!(res.len(), 1);
        assert_eq!(res[0].url, "https://www.a.com");

        let res = searcher
            .search(&SearchQuery {
                query: "website".to_string(),
                optic_program: Some(
                    r#"
                        Rule {
                            Matches {
                                Domain("a.com")
                            },
                            Action(Boost(10))
                        }
                    "#
                    .to_string(),
                ),
                ..Default::default()
            })
            .unwrap()
            .webpages;

        assert_eq!(res.len(), 2);
        assert_eq!(res[0].url, "https://www.a.com");
        assert_eq!(res[1].url, "https://www.b.com");
    }

    #[test]
    fn example_optics_dont_crash() {
        let mut index = Index::temporary().expect("Unable to open index");

        index
            .insert(Webpage {
                html: Html::parse(
                    &format!(
                        r#"
                    <html>
                        <head>
                            <title>Website A</title>
                        </head>
                        <body>
                            {CONTENT}
                            example example example
                        </body>
                    </html>
                "#
                    ),
                    "https://www.a.com/this/is/a/pattern",
                ),
                backlinks: vec![],
                host_centrality: 0.0,
                page_centrality: 0.0,
                fetch_time_ms: 500,
                pre_computed_score: 0.0,
                crawl_stability: 0.0,
                primary_image: None,
                node_id: None,
                host_topic: None,
                dmoz_description: None,
            })
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
                    "https://www.b.com/this/is/b/pattern",
                ),
                backlinks: vec![],
                host_centrality: 0.0001,
                page_centrality: 0.0,
                primary_image: None,
                pre_computed_score: 0.0,
                crawl_stability: 0.0,
                fetch_time_ms: 500,
                node_id: None,
                host_topic: None,
                dmoz_description: None,
            })
            .expect("failed to insert webpage");

        index.commit().expect("failed to commit index");
        let searcher = LocalSearcher::from(index);

        let _ = searcher
            .search(&SearchQuery {
                query: "website".to_string(),
                optic_program: Some(
                    include_str!("../../../optics/testcases/quickstart.optic").to_string(),
                ),
                ..Default::default()
            })
            .unwrap()
            .webpages;

        let _ = searcher
            .search(&SearchQuery {
                query: "website".to_string(),
                optic_program: Some(
                    include_str!("../../../optics/testcases/hacker_news.optic").to_string(),
                ),
                ..Default::default()
            })
            .unwrap()
            .webpages;

        let _ = searcher
            .search(&SearchQuery {
                query: "website".to_string(),
                optic_program: Some(
                    include_str!("../../../optics/testcases/copycats_removal.optic").to_string(),
                ),
                ..Default::default()
            })
            .unwrap()
            .webpages;
    }

    #[test]
    fn empty_discard() {
        let mut index = Index::temporary().expect("Unable to open index");

        index
            .insert(Webpage {
                html: Html::parse(
                    &format!(
                        r#"
                    <html>
                        <head>
                            <title>Website A</title>
                        </head>
                        <body>
                            {CONTENT} {}
                            example example example
                        </body>
                    </html>
                "#,
                        crate::rand_words(100)
                    ),
                    "https://www.a.com/this/is/a/pattern",
                ),
                backlinks: vec![],
                host_centrality: 0.0,
                page_centrality: 0.0,
                fetch_time_ms: 500,
                pre_computed_score: 0.0,
                crawl_stability: 0.0,
                primary_image: None,
                node_id: None,
                host_topic: None,
                dmoz_description: None,
            })
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
                            {CONTENT} {}
                        </body>
                    </html>
                "#,
                        crate::rand_words(100)
                    ),
                    "https://www.b.com/this/is/b/pattern",
                ),
                backlinks: vec![],
                host_centrality: 0.0001,
                page_centrality: 0.0,
                primary_image: None,
                pre_computed_score: 0.0,
                crawl_stability: 0.0,
                fetch_time_ms: 500,
                node_id: None,
                dmoz_description: None,
                host_topic: None,
            })
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
                            {CONTENT} {}
                        </body>
                    </html>
                "#,
                        crate::rand_words(100)
                    ),
                    "https://www.c.com/this/is/c/pattern",
                ),
                backlinks: vec![],
                host_centrality: 0.0001,
                page_centrality: 0.0,
                primary_image: None,
                pre_computed_score: 0.0,
                crawl_stability: 0.0,
                fetch_time_ms: 500,
                node_id: None,
                host_topic: None,
                dmoz_description: None,
            })
            .expect("failed to insert webpage");

        index.commit().expect("failed to commit index");
        let searcher = LocalSearcher::from(index);

        let res = searcher
            .search(&SearchQuery {
                query: "website".to_string(),
                optic_program: Some(
                    r#"
                    DiscardNonMatching;
                    Rule {
                        Matches {
                            Domain("a.com")
                        },
                        Action(Boost(6))
                    };
                    Rule {
                        Matches {
                            Domain("b.com")
                        },
                        Action(Boost(1))
                    };
                "#
                    .to_string(),
                ),
                ..Default::default()
            })
            .unwrap()
            .webpages;

        assert_eq!(res.len(), 2);
        assert_eq!(res[0].url, "https://www.a.com/this/is/a/pattern");
    }

    #[test]
    fn liked_sites() {
        let mut index = Index::temporary().expect("Unable to open index");

        let mut graph = WebgraphBuilder::new_memory().open();

        graph.insert(
            Node::from("https://www.a.com").into_host(),
            Node::from("https://www.b.com").into_host(),
            String::new(),
        );

        graph.insert(
            Node::from("https://www.c.com").into_host(),
            Node::from("https://www.c.com").into_host(),
            String::new(),
        );

        graph.commit();

        let centrality_store = CentralityStore::build(&graph, gen_temp_path());

        index
            .insert(Webpage {
                html: Html::parse(
                    &format!(
                        r#"
                    <html>
                        <head>
                            <title>Website A</title>
                        </head>
                        <body>
                            {CONTENT} {}
                            example example example
                        </body>
                    </html>
                "#,
                        crate::rand_words(100)
                    ),
                    "https://www.a.com/this/is/a/pattern",
                ),
                backlinks: vec![],
                host_centrality: 0.0,
                page_centrality: 0.0,
                fetch_time_ms: 500,
                pre_computed_score: 0.0,
                crawl_stability: 0.0,
                primary_image: None,
                host_topic: None,
                dmoz_description: None,
                node_id: Some(graph.node2id(&Node::from("www.a.com").into_host()).unwrap()),
            })
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
                            {CONTENT} {}
                        </body>
                    </html>
                "#,
                        crate::rand_words(100)
                    ),
                    "https://www.b.com/this/is/b/pattern",
                ),
                backlinks: vec![],
                host_centrality: 0.0001,
                page_centrality: 0.0,
                primary_image: None,
                pre_computed_score: 0.0,
                crawl_stability: 0.0,
                fetch_time_ms: 500,
                dmoz_description: None,
                host_topic: None,
                node_id: Some(graph.node2id(&Node::from("www.b.com").into_host()).unwrap()),
            })
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
                            {CONTENT} {}
                        </body>
                    </html>
                "#,
                        crate::rand_words(100)
                    ),
                    "https://www.c.com/this/is/c/pattern",
                ),
                backlinks: vec![],
                host_centrality: 0.0002,
                page_centrality: 0.0,
                primary_image: None,
                pre_computed_score: 0.0,
                crawl_stability: 0.0,
                fetch_time_ms: 500,
                host_topic: None,
                dmoz_description: None,
                node_id: Some(graph.node2id(&Node::from("www.c.com").into_host()).unwrap()),
            })
            .expect("failed to insert webpage");

        index.commit().expect("failed to commit index");
        let mut searcher = LocalSearcher::from(index);

        searcher.set_centrality_store(centrality_store.into());

        let res = searcher
            .search(&SearchQuery {
                query: "website".to_string(),
                optic_program: Some(
                    r#"
                    Like(Site("www.a.com"));
                    Like(Site("www.b.com"));
                    Dislike(Site("www.c.com"));
                "#
                    .to_string(),
                ),
                ..Default::default()
            })
            .unwrap()
            .webpages;

        assert_eq!(res.len(), 3);
        assert_eq!(res[0].url, "https://www.b.com/this/is/b/pattern");
        assert_eq!(res[1].url, "https://www.a.com/this/is/a/pattern");
        assert_eq!(res[2].url, "https://www.c.com/this/is/c/pattern");
    }

    #[test]
    fn schema_org_search() {
        let mut index = Index::temporary().expect("Unable to open index");

        index
            .insert(Webpage {
                html: Html::parse(
                    &format!(
                        r#"
                    <html>
                        <head>
                            <title>Website A</title>
                            <script type="application/ld+json">
                                {{
                                "@context": "https://schema.org",
                                "@type": "ImageObject",
                                "author": "Jane Doe",
                                "contentLocation": "Puerto Vallarta, Mexico",
                                "contentUrl": "mexico-beach.jpg",
                                "datePublished": "2008-01-25",
                                "description": "I took this picture while on vacation last year.",
                                "name": "Beach in Mexico"
                                }}
                            </script>
                        </head>
                        <body>
                            {CONTENT} {}
                            example example example
                        </body>
                    </html>
                "#,
                        crate::rand_words(100)
                    ),
                    "https://www.a.com/",
                ),
                backlinks: vec![],
                host_centrality: 0.0,
                page_centrality: 0.0,
                fetch_time_ms: 500,
                pre_computed_score: 0.0,
                crawl_stability: 0.0,
                primary_image: None,
                node_id: None,
                host_topic: None,
                dmoz_description: None,
            })
            .expect("failed to insert webpage");
        index
            .insert(Webpage {
                html: Html::parse(
                    &format!(
                        r##"
                    <html>
                        <head>
                            <title>Website B</title>
                        </head>
                        <body>
                            <article itemscope itemtype="http://schema.org/BlogPosting">
                                <section>
                                <h1>Comments</h1>
                                <article itemprop="comment" itemscope itemtype="http://schema.org/UserComments" id="c1">
                                <link itemprop="url" href="#c1">
                                <footer>
                                    <p>Posted by: <span itemprop="creator" itemscope itemtype="http://schema.org/Person">
                                    <span itemprop="name">Greg</span>
                                    </span></p>
                                    <p><time itemprop="commentTime" datetime="2013-08-29">15 minutes ago</time></p>
                                </footer>
                                <p>Ha!</p>
                                </article>
                                </section>
                            </article>
                            {CONTENT} {}
                        </body>
                    </html>
                "##,
                        crate::rand_words(100)
                    ),
                    "https://www.b.com/",
                ),
                backlinks: vec![],
                host_centrality: 0.0001,
                page_centrality: 0.0,
                primary_image: None,
                pre_computed_score: 0.0,
                crawl_stability: 0.0,
                fetch_time_ms: 500,
                node_id: None,
                dmoz_description: None,
                host_topic: None,
            })
            .expect("failed to insert webpage");

        index.commit().unwrap();
        let searcher = LocalSearcher::from(index);

        let res = searcher
            .search(&SearchQuery {
                query: "website".to_string(),
                optic_program: Some(
                    r#"
                        DiscardNonMatching;
                        Rule {
                            Matches {
                                Schema("BlogPosting")
                            }
                        }
                    "#
                    .to_string(),
                ),
                ..Default::default()
            })
            .unwrap()
            .webpages;

        assert_eq!(res.len(), 1);
        assert_eq!(res[0].url, "https://www.b.com/");

        let res = searcher
            .search(&SearchQuery {
                query: "website".to_string(),
                optic_program: Some(
                    r#"
                        DiscardNonMatching;
                        Rule {
                            Matches {
                                Schema("BlogPosting.comment")
                            }
                        }
                    "#
                    .to_string(),
                ),
                ..Default::default()
            })
            .unwrap()
            .webpages;

        assert_eq!(res.len(), 1);
        assert_eq!(res[0].url, "https://www.b.com/");

        let res = searcher
            .search(&SearchQuery {
                query: "website".to_string(),
                optic_program: Some(
                    r#"
                        DiscardNonMatching;
                        Rule {
                            Matches {
                                Schema("ImageObject")
                            }
                        }
                    "#
                    .to_string(),
                ),
                ..Default::default()
            })
            .unwrap()
            .webpages;

        assert_eq!(res.len(), 1);
        assert_eq!(res[0].url, "https://www.a.com/");

        let res = searcher
            .search(&SearchQuery {
                query: "website".to_string(),
                optic_program: Some(
                    r#"
                        DiscardNonMatching;
                        Rule {
                            Matches {
                                Schema("Person")
                            }
                        }
                    "#
                    .to_string(),
                ),
                ..Default::default()
            })
            .unwrap()
            .webpages;

        assert_eq!(res.len(), 1);
        assert_eq!(res[0].url, "https://www.b.com/");
    }

    #[test]
    fn pattern_same_phrase() {
        let mut index = Index::temporary().expect("Unable to open index");

        index
            .insert(Webpage {
                html: Html::parse(
                    &format!(
                        r#"
                    <html>
                        <head>
                            <title>Website A</title>
                        </head>
                        <body>
                            {CONTENT} {}
                            example example example
                        </body>
                    </html>
                "#,
                        crate::rand_words(100)
                    ),
                    "https://chat.stackoverflow.com",
                ),
                backlinks: vec![],
                host_centrality: 0.0,
                page_centrality: 0.0,
                fetch_time_ms: 500,
                pre_computed_score: 0.0,
                crawl_stability: 0.0,
                primary_image: None,
                node_id: None,
                host_topic: None,
                dmoz_description: None,
            })
            .expect("failed to insert webpage");

        index.commit().expect("failed to commit index");
        let searcher = LocalSearcher::from(index);

        let res = searcher
            .search(&SearchQuery {
                query: "site:stackoverflow.com".to_string(),
                optic_program: Some(
                    r#"
                    DiscardNonMatching;
                    Rule {
                        Matches {
                            Site("a.com")
                        },
                        Action(Boost(6))
                    };
                    Rule {
                        Matches {
                            Site("stackoverflow.blog")
                        },
                        Action(Boost(1))
                    };
                    Rule {
                        Matches {
                            Site("chat.b.eu")
                        },
                        Action(Boost(1))
                    };
                "#
                    .to_string(),
                ),
                ..Default::default()
            })
            .unwrap()
            .webpages;

        assert_eq!(res.len(), 0);
    }

    #[test]
    fn discard_all_discard_like() {
        let mut index = Index::temporary().expect("Unable to open index");

        index
            .insert(Webpage {
                html: Html::parse(
                    &format!(
                        r#"
                    <html>
                        <head>
                            <title>Website A</title>
                        </head>
                        <body>
                            {CONTENT} {}
                            example example example
                        </body>
                    </html>
                "#,
                        crate::rand_words(100)
                    ),
                    "https://a.com",
                ),
                backlinks: vec![],
                host_centrality: 0.0,
                page_centrality: 0.0,
                fetch_time_ms: 500,
                pre_computed_score: 0.0,
                crawl_stability: 0.0,
                primary_image: None,
                node_id: None,
                host_topic: None,
                dmoz_description: None,
            })
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
                            {CONTENT} {}
                            example example example
                        </body>
                    </html>
                "#,
                        crate::rand_words(100)
                    ),
                    "https://b.com",
                ),
                backlinks: vec![],
                host_centrality: 0.0,
                page_centrality: 0.0,
                fetch_time_ms: 500,
                pre_computed_score: 0.0,
                crawl_stability: 0.0,
                primary_image: None,
                node_id: None,
                host_topic: None,
                dmoz_description: None,
            })
            .expect("failed to insert webpage");

        index.commit().expect("failed to commit index");
        let searcher = LocalSearcher::from(index);

        let res = searcher
            .search(&SearchQuery {
                query: "example".to_string(),
                optic_program: Some(
                    r#"
                    DiscardNonMatching;
                    Rule {
                        Matches {
                            Site("b.com")
                        }
                    };
                "#
                    .to_string(),
                ),
                site_rankings: Some(SiteRankings {
                    liked: vec!["a.com".to_string()],
                    disliked: vec![],
                    blocked: vec![],
                }),
                ..Default::default()
            })
            .unwrap()
            .webpages;

        assert_eq!(res.len(), 1);
        assert_eq!(res[0].url, "https://b.com");
    }
}
