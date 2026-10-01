//! End-to-end test against a real (dockerized) Jellyfin server: when
//! http://localhost:8096 is reachable, an #[ignore]-gated test does a real
//! initial sync and asserts item counts.
//!
//! The expected movie count is DERIVED from `dev/media/Movies` at runtime
//! rather than hardcoded: the latency benches add git-ignored local
//! fixtures to the corpus, so any fixed number rots on machines that have
//! generated them (the original "33 movies as of corpus v1" did exactly
//! that).
//!
//! Run explicitly (not part of the default `cargo test`):
//!   cargo test -p media-cache --test live_sync -- --ignored --nocapture
//!
//! Requires `dev/server.sh up` (see dev/README.md): creds
//! jellybeam-admin/jellybeam-test against http://localhost:8096 by default;
//! override with `JELLYBEAM_DEV_SERVER_URL` to target another dev server
//! instance (e.g. the opt-in Jellyfin 12.0 server on :8097).

use std::time::Duration;

use jellyfin_api::{ClientIdentity, JellyfinClient};
use jellyfin_core::BusEvent;
use media_cache::{Mirror, Sort};

fn base_url() -> String {
    std::env::var("JELLYBEAM_DEV_SERVER_URL")
        .unwrap_or_else(|_| "http://localhost:8096".to_string())
}

/// Count the video files actually present in `dev/media/Movies` -- the same
/// set the dockerized server scans (sidecar files like `.srt` don't become
/// items). See the module doc for why this isn't a hardcoded number.
fn corpus_movie_count() -> usize {
    const VIDEO_EXTS: &[&str] = &["mkv", "mp4", "ts", "avi", "webm", "m4v", "mov"];
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dev/media/Movies");
    std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read corpus dir {}: {e}", dir.display()))
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .path()
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| VIDEO_EXTS.contains(&ext.to_lowercase().as_str()))
        })
        .count()
}

fn identity() -> ClientIdentity {
    ClientIdentity {
        client: "Jellybeam Live Test".to_string(),
        device: "test-runner".to_string(),
        device_id: "jellybeam-live-sync-test".to_string(),
        version: "0.1.0".to_string(),
    }
}

/// Poll a mirror-derived count until it stabilizes at (or exceeds) the
/// expected value, or time out. Initial sync runs on a background task
/// (`Mirror::open` doesn't await it -- Home is meant to render progressively
/// per docs/DATA.md §2), so the test has to poll rather than assume it's done the
/// moment `open()` returns.
async fn wait_for<F: Fn() -> usize>(
    what: &str,
    expected: usize,
    timeout: Duration,
    poll: F,
) -> usize {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let n = poll();
        if n >= expected || tokio::time::Instant::now() >= deadline {
            return n;
        }
        tracing::debug!(what, n, expected, "waiting for sync to catch up");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

#[tokio::test]
#[ignore = "requires a live Jellyfin server at localhost:8096 (see dev/README.md: dev/server.sh up)"]
async fn initial_sync_against_live_server_matches_corpus_counts() {
    let (client, _auth) = JellyfinClient::authenticate_by_name(
        &base_url(),
        identity(),
        "jellybeam-admin",
        "jellybeam-test",
    )
    .await
    .expect("authenticate against dockerized Jellyfin server (is `dev/server.sh up` running?)");

    let (_bus_tx, bus_rx) = tokio::sync::broadcast::channel::<BusEvent>(16);

    let dir = tempfile::tempdir().expect("tempdir");
    let mirror = Mirror::open(dir.path().to_path_buf(), client, bus_rx)
        .await
        .expect("open mirror");

    // Views (Movies + Shows libraries) land first.
    let views = wait_for("views", 2, Duration::from_secs(15), || mirror.views().len()).await;
    assert!(
        views >= 2,
        "expected at least Movies + Shows views, got {views}"
    );

    let (movies_id, shows_id) = {
        let views = mirror.views();
        let movies = views
            .iter()
            .find(|v| v.name == "Movies")
            .map(|v| v.id.clone());
        let shows = views
            .iter()
            .find(|v| v.name == "Shows")
            .map(|v| v.id.clone());
        (
            movies.unwrap_or_else(|| panic!("no 'Movies' view among {views:?}")),
            shows.unwrap_or_else(|| panic!("no 'Shows' view among {views:?}")),
        )
    };

    // Movies are direct children of the Movies library folder.
    let expected_movies = corpus_movie_count();
    let movie_count = wait_for("movies", expected_movies, Duration::from_secs(30), || {
        mirror.children(&movies_id, Sort::NameAsc, 0, 200).len()
    })
    .await;
    assert_eq!(
        movie_count, expected_movies,
        "server's movie count should match the video files in dev/media/Movies \
         (has the server library-scanned recently added local fixtures? \
         dev/setup-server.sh triggers a scan)"
    );

    // Episodes nest under Series -> Season, so they don't show up as direct
    // children of the Shows view; `latest()` is the mirror's own path to
    // "recently added" for it, and doubles as an assertion that view ->
    // item_type mapping + the breadth sync of nested items both work end to
    // end. For a `tvshows` view this is series-grouped (Jellyfin's
    // own `GroupItems=true` Latest-Media semantics -- see
    // `query::latest_grouped_series`), so corpus v1's 4 episodes across 2
    // series surface as 2 rows here, not 4; the raw episode count is
    // asserted separately below via `children(series_id)` ->
    // `children(season_id)`.
    let series_latest_count = wait_for(
        "shows latest (series-grouped)",
        2,
        Duration::from_secs(30),
        || mirror.latest(&shows_id, 200, false).len(),
    )
    .await;
    assert_eq!(
        series_latest_count, 2,
        "corpus v1's Shows Latest must show one row per series (2 series), not one per episode"
    );
    assert!(
        mirror
            .latest(&shows_id, 200, false)
            .iter()
            .all(|r| r.item_type == "Series"),
        "every row of a tvshows view's Latest shelf must be the series' own row"
    );

    // `parent_id` requires requesting `ParentId` explicitly -- the server
    // only returns it when asked (see `rows::browse_parent_id`); without
    // it, `Mirror::children` on a
    // Series or Season id would return nothing. Assert the actual browse path
    // works end to end against the real server response: each of the two
    // Series under Shows must expose its seasons via `children(series_id)`,
    // and each season found that way must expose its episodes via
    // `children(season_id)` -- summing to all 4 corpus episodes.
    let series_rows = mirror.children(&shows_id, Sort::NameAsc, 0, 200);
    assert_eq!(
        series_rows.len(),
        2,
        "corpus v1 has 2 series directly under Shows, got {series_rows:?}"
    );

    let mut total_episodes_via_children = 0usize;
    for series in &series_rows {
        let seasons = wait_for(
            &format!("seasons of {}", series.name),
            1,
            Duration::from_secs(15),
            || mirror.children(&series.id, Sort::NameAsc, 0, 200).len(),
        )
        .await;
        assert!(
            seasons >= 1,
            "series {:?} ({}) should have at least one season as a child, got 0",
            series.name,
            series.id
        );

        let season_rows = mirror.children(&series.id, Sort::NameAsc, 0, 200);
        for season in &season_rows {
            let episodes = mirror.children(&season.id, Sort::NameAsc, 0, 200);
            assert!(
                !episodes.is_empty(),
                "season {:?} ({}) under series {:?} should have at least one episode as a child, got 0",
                season.name,
                season.id,
                series.name
            );
            total_episodes_via_children += episodes.len();
        }
    }
    assert_eq!(
        total_episodes_via_children, 4,
        "children(series_id) -> children(season_id) should reach all 4 corpus episodes"
    );

    // Search should find at least one of the distinctly-titled corpus files.
    let hits = mirror.search("Quantum", 10);
    assert!(
        !hits.is_empty(),
        "expected to find 'Quantum Static' via search, got no hits"
    );

    // Detail view: blob round-trips through the mirror.
    let some_movie_id = mirror
        .children(&movies_id, Sort::NameAsc, 0, 1)
        .first()
        .map(|c| c.id.clone());
    if let Some(id) = some_movie_id {
        let dto = mirror.item(&id);
        assert!(
            dto.is_some(),
            "item() should resolve the full DTO for a synced item"
        );
    }

    // `library_id` isn't part of the `Mirror`/`CardRow` public surface
    // (docs/DATA.md's "interfaces frozen" -- it's purely a query-scoping column),
    // so verifying it end to end against the real server means reading the
    // mirror's own SQLite file directly rather than going through `Mirror`.
    // `rusqlite` is already a normal (non-dev) dependency of `media-cache`,
    // so it's available here the same way it is inside the crate.
    {
        let db_path = dir.path().join("mirror.db");
        let conn = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("open mirror db directly for library_id assertions");

        let unstamped: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM items WHERE library_id IS NULL",
                [],
                |r| r.get(0),
            )
            .expect("count unstamped items");
        assert_eq!(
            unstamped, 0,
            "every synced item must carry a library_id after a full initial sync \
             (breadth sync stamps every item in a view's recursive page)"
        );

        // Scoped to `item_type = 'Movie'` (rather than counting every row
        // stamped with this library's id outright): the Movies view's
        // recursive breadth-sync page can legitimately include non-`Movie`
        // rows too (e.g. an intermediate physical-folder item) that are
        // correctly stamped with this same library id -- this assertion
        // cares specifically about matching `movie_count` above (via
        // `children()`), not about the page's exact row count.
        let movies_scoped: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM items WHERE library_id = ?1 AND item_type = 'Movie'",
                [&movies_id],
                |r| r.get(0),
            )
            .expect("count movies-scoped items");
        assert_eq!(
            movies_scoped as usize,
            corpus_movie_count(),
            "all corpus movies must be stamped with the Movies view's id \
             (count derived from dev/media/Movies -- see module doc)"
        );

        let shows_scoped: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM items WHERE library_id = ?1 AND item_type = 'Episode'",
                [&shows_id],
                |r| r.get(0),
            )
            .expect("count shows-scoped episodes");
        assert_eq!(
            shows_scoped, 4,
            "all 4 corpus episodes must be stamped with the Shows view's id"
        );
    }

    // Two different views' `latest()` must return disjoint id
    // sets -- scoping by `item_type` alone would let any two libraries
    // show identical "Latest" shelves. Movies vs Shows is the
    // strongest disjointness check the single-Shows-library dev corpus can
    // offer end to end (see the module doc comment on this test file for
    // why a second same-`collection_type` library isn't constructed here --
    // that scoping property is covered directly against a mocked mirror in
    // `media-cache/src/query.rs`'s `latest_scopes_by_library_not_just_item_type`).
    let movies_latest = mirror.latest(&movies_id, 200, false);
    let shows_latest = mirror.latest(&shows_id, 200, false);
    assert!(
        !movies_latest.is_empty(),
        "Movies library should have a Latest shelf"
    );
    assert!(
        !shows_latest.is_empty(),
        "Shows library should have a Latest shelf"
    );
    let movies_latest_ids: std::collections::HashSet<&str> =
        movies_latest.iter().map(|r| r.id.as_str()).collect();
    let shows_latest_ids: std::collections::HashSet<&str> =
        shows_latest.iter().map(|r| r.id.as_str()).collect();
    assert!(
        movies_latest_ids.is_disjoint(&shows_latest_ids),
        "Movies and Shows Latest shelves must never share an id: \
         movies={movies_latest_ids:?} shows={shows_latest_ids:?}"
    );

    // The Shows library's Latest shelf must be
    // series-grouped (one tile per series, Jellyfin's own
    // `GroupItems=true` semantics), not one tile per episode -- corpus v1
    // has 4 episodes across 2 series, so a per-episode shelf would show 4
    // rows instead of 2.
    assert_eq!(
        shows_latest.len(),
        2,
        "Shows Latest must show one row per series (2 series, 4 episodes), not one per episode; got {shows_latest:?}"
    );
    assert!(
        shows_latest.iter().all(|r| r.item_type == "Series"),
        "every row in a tvshows view's Latest shelf must be the series' own row, got {shows_latest:?}"
    );
}
