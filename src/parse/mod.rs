//! Forum-software parsers. Each turns one HTML page into the normalized model.

pub mod xenforo;

use anyhow::Result;
use scraper::Html;

use crate::model::Page;

/// Forum software we can parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Software {
    /// Detect from the page.
    Auto,
    Xenforo,
}

/// Parse a page with the given (or detected) parser.
pub fn parse_page(html: &str, url: &str, software: Software) -> Result<Option<Page>> {
    match software {
        Software::Xenforo => xenforo::parse(html, url),
        Software::Auto => {
            let doc = Html::parse_document(html);
            if xenforo::detect(&doc) {
                xenforo::parse(html, url)
            } else {
                Ok(None)
            }
        }
    }
}
