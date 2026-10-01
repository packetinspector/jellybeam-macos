//! View map + per-pane back/forward history (docs/UX-SPEC.md §1: "Back/forward
//! history per pane (⌘[ / ⌘])"). Depth is at most Library → Detail, so a
//! plain two-stack history (no tree) is sufficient.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum View {
    Home,
    Library {
        view_id: String,
    },
    /// A `ViewKind::Channel` view (docs/PLUGIN-CHANNELS.md
    /// §2.1/§2.2), browsed live rather than through the mirror --
    /// `channel_browse.rs` owns the actual fetched rows. `folder_id: None` is
    /// level 1 (the view itself, `ChannelFolderItem` folders); `Some(id)` is
    /// level 2 (that folder's recordings). Encoding the level in `View`
    /// itself (rather than only in `channel_browse::ChannelBrowseState`)
    /// means opening a folder is an ordinary `Nav::go` -- ordinary back/
    /// forward history "just works" for folder -> view -> previous screen,
    /// with no special-cased pop logic needed here.
    Channel {
        view_id: String,
        folder_id: Option<String>,
    },
    Detail {
        item_id: String,
    },
    /// The Discover (Seerr) section -- a live,
    /// never-mirrored screen family exactly like `View::Channel` above (no
    /// parent linkage a sync walk could place, content that churns on a
    /// separate server's own schedule). `DiscoverView` picks which of
    /// Discover's own sub-screens is showing; `discover.rs` owns the actual
    /// fetched state, keyed by session-lifetime rather than by this nav
    /// entry (so switching between two Detail pages, say, doesn't need a
    /// third `View` variant per sub-screen the way Channel's two levels do).
    Discover(DiscoverView),
}

/// One of Discover's sub-screens (Home shelves, Browse grids, Search,
/// Detail, My Requests, Person). Kept
/// dependency-free of `seerr_api` (plain primitives only) for the same
/// reason `View::Channel`'s `folder_id` is a bare `String` rather than a
/// `ChannelFolderItem` -- `nav.rs` stays a small, standalone module with no
/// knowledge of any one screen's own data shapes; `discover.rs` maps these
/// plain ids to/from the crate's real types at its own boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiscoverView {
    Home,
    BrowseMovies,
    BrowseTv,
    Search,
    MyRequests,
    Detail {
        media_type: DiscoverMediaType,
        tmdb_id: i64,
    },
    Person {
        person_id: i64,
    },
}

/// Mirrors `seerr_api::SeerrMediaType` one-for-one (see `DiscoverView`'s doc
/// comment for why `nav.rs` doesn't just reuse that type directly).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiscoverMediaType {
    Movie,
    Tv,
}

pub(crate) struct Nav {
    pub current: View,
    back: Vec<View>,
    forward: Vec<View>,
}

impl Nav {
    pub(crate) fn new() -> Self {
        Nav {
            current: View::Home,
            back: Vec::new(),
            forward: Vec::new(),
        }
    }

    /// Navigate forward to `view`, pushing the current view onto the back
    /// stack and clearing the forward stack (standard browser-history
    /// semantics: a fresh navigation invalidates any redo path). A no-op if
    /// `view` is already current.
    pub(crate) fn go(&mut self, view: View) {
        if view == self.current {
            return;
        }
        self.forward.clear();
        self.back.push(std::mem::replace(&mut self.current, view));
    }

    /// Swaps `current` in place, touching neither the back nor forward
    /// stack -- unlike `go`, this is for navigation that should feel
    /// instantaneous and not pollute history (see
    /// docs/DESIGN-PLAYER-NAV.md Part 2, Episode Detail page: clicking
    /// through sibling episodes one after another shouldn't make Back walk
    /// through every episode visited; it should return to wherever the
    /// viewer navigated *into* the episode from). A no-op if `view` is
    /// already current.
    pub(crate) fn replace(&mut self, view: View) {
        self.current = view;
    }

    /// ⌘[ / Esc-as-back. Returns `true` if it actually moved.
    pub(crate) fn back(&mut self) -> bool {
        match self.back.pop() {
            Some(prev) => {
                self.forward
                    .push(std::mem::replace(&mut self.current, prev));
                true
            }
            None => false,
        }
    }

    /// ⌘]. Returns `true` if it actually moved.
    pub(crate) fn forward(&mut self) -> bool {
        match self.forward.pop() {
            Some(next) => {
                self.back.push(std::mem::replace(&mut self.current, next));
                true
            }
            None => false,
        }
    }

    pub(crate) fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub(crate) fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_pushes_history_and_clears_forward() {
        let mut nav = Nav::new();
        nav.go(View::Library {
            view_id: "lib1".into(),
        });
        nav.go(View::Detail {
            item_id: "item1".into(),
        });
        assert_eq!(
            nav.current,
            View::Detail {
                item_id: "item1".into()
            }
        );
        assert!(nav.back());
        assert_eq!(
            nav.current,
            View::Library {
                view_id: "lib1".into()
            }
        );
        assert!(nav.can_go_forward());
        assert!(nav.forward());
        assert_eq!(
            nav.current,
            View::Detail {
                item_id: "item1".into()
            }
        );
    }

    #[test]
    fn back_at_root_is_a_noop() {
        let mut nav = Nav::new();
        assert!(!nav.back());
        assert_eq!(nav.current, View::Home);
    }

    #[test]
    fn go_same_view_is_a_noop() {
        let mut nav = Nav::new();
        nav.go(View::Home);
        assert!(!nav.can_go_back());
    }

    #[test]
    fn replace_swaps_current_without_touching_history() {
        let mut nav = Nav::new();
        nav.go(View::Library {
            view_id: "lib1".into(),
        });
        assert!(nav.can_go_back());

        // Simulates clicking through several sibling episodes on the
        // Episode Detail page: each `replace` call must not grow `back`.
        nav.replace(View::Detail {
            item_id: "episode-1".into(),
        });
        nav.replace(View::Detail {
            item_id: "episode-2".into(),
        });
        nav.replace(View::Detail {
            item_id: "episode-3".into(),
        });
        assert_eq!(
            nav.current,
            View::Detail {
                item_id: "episode-3".into()
            }
        );

        // Back must land straight on Home (`back` held only the one real
        // `go` call's previous view), not unwind through episode-1/
        // episode-2 first -- `replace` must never have pushed either onto
        // `back`.
        assert!(nav.back());
        assert_eq!(nav.current, View::Home);
        assert!(!nav.can_go_back());
    }

    #[test]
    fn new_navigation_clears_forward_stack() {
        let mut nav = Nav::new();
        nav.go(View::Library {
            view_id: "a".into(),
        });
        nav.back();
        assert!(nav.can_go_forward());
        nav.go(View::Library {
            view_id: "b".into(),
        });
        assert!(!nav.can_go_forward());
    }

    /// docs/PLUGIN-CHANNELS.md §2.2's "Esc/back pops
    /// folder -> view -> previous screen": since a folder open is just a
    /// `View::Channel` with a different `folder_id`, this needs no special
    /// handling in `Nav` at all -- two plain `back()` calls already unwind
    /// it in the right order, the same as any other two-deep `go` sequence.
    #[test]
    fn channel_folder_back_pops_to_the_view_then_to_the_previous_screen() {
        let mut nav = Nav::new();
        nav.go(View::Library {
            view_id: "movies".into(),
        });
        nav.go(View::Channel {
            view_id: "recordings".into(),
            folder_id: None,
        });
        nav.go(View::Channel {
            view_id: "recordings".into(),
            folder_id: Some("day-1".into()),
        });

        assert!(nav.back());
        assert_eq!(
            nav.current,
            View::Channel {
                view_id: "recordings".into(),
                folder_id: None,
            },
            "first back pops the folder, landing on the view itself"
        );

        assert!(nav.back());
        assert_eq!(
            nav.current,
            View::Library {
                view_id: "movies".into()
            },
            "second back pops the view, landing on the previous screen"
        );
    }
}
