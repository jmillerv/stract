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

pub struct Collector;

impl Collector {
    pub fn site_penalty() -> f64 {
        0.1
    }

    pub fn title_penalty() -> f64 {
        1.0
    }

    pub fn url_penalty() -> f64 {
        20.0
    }

    pub fn url_without_tld_penalty() -> f64 {
        1.0
    }

    pub fn max_docs_considered() -> usize {
        250_000
    }
}

pub struct Api;

impl Api {
    pub fn stackoverflow() -> f64 {
        0.5
    }

    pub fn entity_sidebar() -> f64 {
        10.0
    }
}

pub struct Snippet;

impl Snippet {
    pub fn desired_num_chars() -> usize {
        275
    }

    pub fn delta_num_chars() -> usize {
        50
    }

    pub fn min_passage_width() -> usize {
        20
    }

    pub fn empty_query_snippet_words() -> usize {
        50
    }

    pub fn min_description_words() -> usize {
        10
    }

    pub fn min_body_length() -> usize {
        256
    }

    pub fn min_body_length_homepage() -> usize {
        1024
    }
}

pub struct Crawler;

impl Crawler {
    pub fn robots_txt_cache_sec() -> u64 {
        60 * 60
    }

    pub fn politeness_factor() -> f32 {
        1.0
    }

    pub fn min_crawl_delay_ms() -> u64 {
        5_000
    }

    pub fn max_crawl_delay_ms() -> u64 {
        60_000
    }

    pub fn max_politeness_factor() -> f32 {
        2048.0
    }

    pub fn max_url_slowdown_retry() -> u8 {
        3
    }

    pub fn max_redirects() -> usize {
        5
    }

    pub fn dry_run() -> bool {
        false
    }
}

pub struct WebgraphServer;

impl WebgraphServer {
    pub fn max_similar_hosts() -> usize {
        1_000
    }
}

pub struct SearchQuery;

impl SearchQuery {
    pub fn flatten_response() -> bool {
        true
    }

    pub fn return_ranking_signals() -> bool {
        false
    }

    pub fn safe_search() -> bool {
        false
    }

    pub fn count_results() -> bool {
        false
    }
}

pub struct Correction;

impl Correction {
    pub fn misspelled_prob() -> f64 {
        0.1
    }

    pub fn correction_threshold() -> f64 {
        50.0 // logprob difference
    }

    pub fn lm_prob_weight() -> f64 {
        5.77
    }
}

pub struct Widgets;

impl Widgets {
    pub fn calculator_fetch_currencies_exchange() -> bool {
        true
    }
}
