//! Tests against the real ListenBrainz and MusicBrainz APIs.
//! Ignored by default so `cargo test` works offline (and inside the Nix sandbox);
//! CI runs them with `cargo test -- --include-ignored`.

use tapedeck::{listenbrainz::ListenBrainz, musicbrainz::MusicBrainz};

fn http() -> reqwest::Client {
    reqwest::Client::builder().user_agent(tapedeck::USER_AGENT).build().unwrap()
}

#[tokio::test]
#[ignore = "needs network"]
async fn listenbrainz_created_for_and_tracks() {
    let lb = ListenBrainz::new(http(), None);
    let playlists = lb.created_for("rob").await.unwrap();
    let weekly = playlists.iter().find(|p| p.kind == "weekly-jams").expect("rob has weekly jams");
    let tracks = lb.tracks(&weekly.mbid).await.unwrap();
    assert!(!tracks.is_empty());
    assert_eq!(tracks[0].recording_mbid.len(), 36, "{:?}", tracks[0]);
}

#[tokio::test]
#[ignore = "needs network"]
async fn musicbrainz_recording_lookup() {
    // Bicep - Apricots
    let r = MusicBrainz::new(http()).recording("55ae2a14-acdc-4a9d-9888-1cf548964d17").await.unwrap();
    assert_eq!(r.title, "Apricots");
    assert_eq!(r.album_artist, "Bicep");
    assert_eq!(r.album, "Isles");
    assert_eq!(r.year, Some(2021));
    // Which edition of Isles wins can vary with release ordering; only insist a position exists.
    assert!(r.track_no.is_some());
}
