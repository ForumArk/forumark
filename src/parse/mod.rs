//! Forum-software parsers. Each turns one HTML page into the normalized model.

pub mod phpbb;
pub mod smf;
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
    /// phpBB 2 and 3.
    Phpbb,
    /// SMF (Simple Machines Forum) 1.x and 2.x.
    Smf,
}

/// Parse a page with the given (or detected) parser.
pub fn parse_page(html: &str, url: &str, software: Software) -> Result<Option<Page>> {
    match software {
        Software::Xenforo => xenforo::parse(html, url),
        Software::Phpbb => phpbb::parse(html, url),
        Software::Smf => smf::parse(html, url),
        Software::Auto => {
            let doc = Html::parse_document(html);
            // Structural XenForo markers first, then phpBB, then the looser XenForo check
            // (a phpBB post that merely mentions "XenForo" must stay phpBB).
            if xenforo::detect_strong(&doc) {
                xenforo::parse(html, url)
            } else if phpbb::detect(&doc) {
                phpbb::parse(html, url)
            } else if smf::detect(&doc) {
                smf::parse(html, url)
            } else if xenforo::detect(&doc) {
                xenforo::parse(html, url)
            } else {
                Ok(None)
            }
        }
    }
}
