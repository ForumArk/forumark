# Forum Ark

**Forums shut down. Their knowledge doesn't have to.**

Forum Ark turns a forum into a permanent, searchable **static website**: plain HTML files you can host anywhere (a cheap web host, GitHub Pages, a USB stick) that will still work in 20 years. No database, no server code, no accounts.

It can rebuild a forum that is already gone, from the copies saved by the Internet Archive's **Wayback Machine**.

Live example: the RPG Maker Forums archive at [thecodelair.com](https://thecodelair.com) (being restored).

## What you get

- Every thread, page by page, with authors, dates and posts, plus forum listings and member pages.
- Full-text search that runs in the browser ([Pagefind](https://pagefind.app)), with no search server needed.
- Embedded videos and posts (YouTube, Imgur, X/Twitter, Pastebin, …) become links to the original, and old smilies become emoji.
- Clean, readable pages with light/dark mode and a larger-text setting, and a layout that works on phones.
- Stable links: `threads/<id>/page-N/`. Old forum links inside posts are rewritten to point inside the archive.
- Pages that were never captured still exist, with a "not preserved" note and a link to the Wayback Machine, so links don't break.
- **Images and attachments are not copied.** They link to the Wayback Machine's own copies. That keeps your archive small and avoids re-hosting other people's files.

Supported forum software today: **XenForo 1 and 2**. More parsers are welcome (see *Contributing*).

## Download

Grab the file for your system from the [latest release](https://github.com/ForumArk/forumark/releases/latest). It's one file with nothing to install.

| System | File |
|---|---|
| Windows | `forumark-windows-x86_64.zip` |
| macOS (Apple Silicon) | `forumark-macos-arm64.tar.gz` |
| Linux | `forumark-linux-x86_64.tar.gz` |

Or build it yourself with [Rust](https://rustup.rs): `cargo build --release` (the program ends up in `target/release/forumark`).

## Quick start: rebuild a forum from the Wayback Machine

Everything is stored in one database file (`archive.db` by default, change it with `--db`). Each step can be stopped and restarted. It picks up where it left off.

```bash
# 1. List every saved thread and forum page (can take a while for big forums)
forumark wayback list --host forums.example.com

# 2. Download them, politely (3 at a time, 1 second apart)
forumark wayback fetch --raw-dir raw

# 3. Try failed pages once more, gently
forumark wayback fetch --raw-dir raw --retry-errors --workers 2 --delay-ms 2000

# 4. See what you have
forumark wayback status
forumark stats

# 5. Build the website
forumark build --out site \
  --origin https://forums.example.com \
  --title "Example Forums Archive" \
  --contact you@example.org
```

Open `site/index.html` through any web server (for example `python3 -m http.server -d site`) and you're done. Upload the `site` folder anywhere.

Try it small first: `forumark wayback fetch --limit 50` fetches just 50 pages so you can check the result.

## Other ways in

If you saved pages yourself (for example with "Save page as…" in your browser), import them directly:

```bash
forumark import-html saved/*.html
forumark import-html page.html --url https://forums.example.com/threads/hello.123/ --captured-at 20240131120000
```

When the same page was saved more than once, **the newest copy wins**.

## Options for `build`

| Option | What it does |
|---|---|
| `--out DIR` | Where the website goes (default `site`). |
| `--origin URL` | The forum's original address. Used for "original page" links and rewriting old links. **Required.** |
| `--title TEXT` | Name shown on every page. |
| `--contact EMAIL` | Where people can ask for their posts to be removed. **Required.** |
| `--base /path/` | Set this if the archive lives in a sub-folder of your site, e.g. `/myforum/`. |
| `--removal-url`, `--privacy-url`, `--about-url` | Links to your own policy pages, shown in the footer. |
| `--notice TEXT` | Footer notice (default: "unofficial, read-only archive"). |
| `--no-search` | Skip the search index (faster test builds). |
| `--search-chunk N` | Thread pages per search index part (default 12000). Big forums are indexed in parts so building stays within a few GB of memory; lower it if your computer runs out of memory. |

Run `forumark --help` or `forumark <command> --help` for everything else.

## Please archive responsibly

Forum Ark is built to be a good guest. Please keep it that way:

- **Respect `robots.txt` and the forum owner's wishes.** If a live forum blocks crawlers, don't crawl it. Use the Wayback Machine's existing copies or ask the owner. Forum Ark only downloads from the Wayback Machine, never from the live forum.
- **Be gentle with the Internet Archive.** It's a non-profit. Keep the default speed (3 workers, 1 second delay) or slower.
- **Only public content.** Never archive private messages, members-only areas, email addresses or IP addresses.
- **Remove posts when asked**, quickly and without argument, and give people a working contact address (`--contact`).
- **Don't use it commercially or claim to be the original forum.** Say clearly that it's an unofficial archive.
- Posts belong to the people who wrote them. Depending on where you live, privacy law (like the GDPR in the EU) may apply to you. This isn't legal advice.

## How it works

```
Wayback CDX list ──▶ polite fetch ──▶ parser (XenForo 1/2) ──▶ SQLite (newest capture wins) ──▶ static site + search index
```

- `src/wayback.rs`: lists captures through the CDX API and fetches them with retries and backoff (the Wayback Machine is often briefly offline).
- `src/parse/`: one parser per forum software, turning a page into threads, posts, forums and members.
- `src/store.rs`: the SQLite archive. A forum-listing row never overwrites data from the thread page itself.
- `src/sanitize.rs`: cleans post HTML and rewrites links (internal links stay inside the archive, images and files go to the Wayback Machine).
- `src/site.rs` + `templates/`: builds the website. Templates and CSS are compiled into the program.

Run the tests with `cargo test`.

## Contributing

Bug reports and new forum parsers (phpBB, vBulletin, Discourse, SMF, …) are very welcome. A parser is one file in `src/parse/` plus a few saved example pages in `tests/fixtures/`. Please use made-up example content in fixtures, not real people's posts.

**Is your forum closing?** Open an issue with its address and closing date.

## License

[MIT](LICENSE). Use it, change it, share it.
