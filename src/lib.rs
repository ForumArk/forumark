//! Forum Ark: turn forum pages from any source into a permanent, searchable static site.

pub mod embeds;
pub mod model;
pub mod parse;
pub mod removals;
pub mod sanitize;
pub mod site;
pub mod store;
pub mod tidy;
pub mod urls;
pub mod wayback;

/// Where the Forum Ark project lives; linked from every built site and sent in the User-Agent.
pub const PROJECT_URL: &str = "https://github.com/ForumArk/forumark";
