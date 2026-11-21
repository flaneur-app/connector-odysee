// SPDX-License-Identifier: AGPL-3.0-or-later

//! Prints the home feed, a search and the streams of one video, live from Odysee.
//!
//! ```text
//! cargo run --example odysee [query]
//! ```

use flaneur_connector::{Connector, FeedKind, SearchFilters, SearchResult};
use flaneur_connector_odysee::Odysee;
use flaneur_model::Item;

fn line(item: &Item) -> String {
    let channel = item.channel.as_ref().map_or("?", |c| c.name.as_str());
    let duration = item.duration.map_or_else(
        || "--:--".to_owned(),
        |d| format!("{}:{:02}", d / 60, d % 60),
    );
    format!(
        "{:<8} {:>6}  {}  ({channel})",
        format!("{:?}", item.kind),
        duration,
        item.title
    )
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let query = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "linux".to_owned());
    let odysee = Odysee::new()?;

    let feeds = odysee.feeds().expect("feeds");
    let home = feeds.feed(FeedKind::Home, None).await?;
    println!("== Home, first category ({} items)", home.items.len());
    for item in home.items.iter().take(5) {
        println!("  {}", line(item));
    }

    let results = odysee
        .search()
        .expect("search")
        .search(&query, &SearchFilters::default(), None)
        .await?;
    println!("\n== Search {query:?} ({} results)", results.items.len());
    let mut first_video = None;
    for result in results.items.iter().take(8) {
        match result {
            SearchResult::Item(item) => {
                println!("  {}", line(item));
                first_video.get_or_insert_with(|| item.r#ref.clone());
            }
            SearchResult::Channel(channel) => println!("  Channel  {}", channel.name),
            SearchResult::Playlist(playlist) => println!("  Playlist {}", playlist.title),
        }
    }

    let Some(video) = first_video else {
        return Ok(());
    };
    let playable = odysee.player().resolve(&video).await?;
    println!("\n== Resolve {}", playable.item.title);
    println!(
        "  likes {:?}, dislikes {:?}, published {:?}",
        playable.item.reactions.likes,
        playable.item.reactions.dislikes,
        playable.item.published.map(|p| p.at)
    );
    for stream in &playable.streams {
        println!(
            "  {:?} {:?} {}x{} {:?} bytes  {}",
            stream.kind,
            stream.container,
            stream.width.unwrap_or(0),
            stream.height.unwrap_or(0),
            stream.size,
            stream.url
        );
    }
    match &playable.hls_manifest {
        Some(hls) => println!("  HLS {hls}"),
        None => println!("  HLS: not transcoded yet"),
    }
    if let Some(web) = &playable.item.web_url {
        println!("  web {web}");
    }
    Ok(())
}
