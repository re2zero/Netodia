//! `PlaylistDetail.*` callbacks: close-detail, drag-reorder, the edit-artwork picker open and
//! track removal here, the rest through [`crate::ui::callbacks::track_detail`].

use std::sync::Arc;

use slint::{ComponentHandle, Global as _};

use super::artwork::open_edit_artwork;
use crate::ui::callbacks::track_detail;
use crate::ui::callbacks::{DialogClaim, collect_track_ids};
use crate::ui::playlists::{self as playlists_ui_mod, PlaylistsUi};
use netodia_app::library;
use netodia_app::state::AppState;
use netodia_core::error::describe;
use netodia_engine::player::engine::state::lock_state;
use netodia_ui::{AppWindow, PlaylistDetail};

/// Wire the `PlaylistDetail` callbacks. See [`super::wire`].
pub(super) fn wire(ui: &AppWindow, state: &AppState, playlists_ui: &Arc<PlaylistsUi>) {
    track_detail::wire(&ui.global::<PlaylistDetail>().as_weak(), state, playlists_ui);
    wire_cloud_playback(ui, state, playlists_ui);
    wire_close(ui, state, playlists_ui);
    wire_reorder(ui, state, playlists_ui);
    wire_edit_artwork(ui, state, playlists_ui);
    wire_remove_track(ui, state, playlists_ui);
}

/// Cloud playlist rows do not exist in `SQLite`, so activation and shuffle use the buffered
/// `NetEase` source instead of the local queue API.
fn wire_cloud_playback(ui: &AppWindow, state: &AppState, playlists_ui: &Arc<PlaylistsUi>) {
    let s = state.clone();
    let pu = playlists_ui.clone();
    ui.global::<PlaylistDetail>().on_play_cloud_row(move |remote_id, _index| {
        let Some(song_id) = crate::ui::util::parse_cloud_id(remote_id.as_str()) else {
            log::warn!("playlists::play_cloud_song: invalid NetEase song ID");
            return;
        };
        play_cloud_song(&s, pu.detail.cache.track_ids(), song_id, false);
    });

    let s = state.clone();
    let pu = playlists_ui.clone();
    ui.global::<PlaylistDetail>().on_shuffle_all(move || {
        use rand::RngExt;
        let ids = pu.detail.cache.track_ids();
        if ids.is_empty() {
            return;
        }
        let id = ids[rand::rng().random_range(..ids.len())];
        play_cloud_song(&s, ids, id, true);
    });
}

fn play_cloud_song(state: &AppState, ids: Vec<i64>, song_id: i64, shuffle: bool) {
    if song_id <= 0 {
        return;
    }
    let state = state.clone();
    let runtime = state.runtime.clone();
    runtime.spawn(async move {
        let (volume, speed) = {
            let player = lock_state(&state.player_state);
            (player.effective_volume(), player.playback_speed)
        };
        if let Err(error) =
            state.cloud_playback.play_queue(&ids, song_id, shuffle, volume, speed).await
        {
            log::warn!("playlists::play_cloud_song({song_id}): {}", describe(&error));
        }
    });
}

/// The header's back button.
///
/// No `mark_drill_back`, where its three siblings have one: theirs precedes a `return_to_section`
/// that flips the nav index, and this detail records no origin at all. The body that mounts in
/// its place takes a fixed `below` and reads the global not at all; see `ui::nav_transition`.
fn wire_close(ui: &AppWindow, state: &AppState, playlists_ui: &Arc<PlaylistsUi>) {
    let s = state.clone();
    let pu = playlists_ui.clone();
    let weak = ui.as_weak();
    ui.global::<PlaylistDetail>().on_close_detail(move || {
        let Some(ui) = weak.upgrade() else { return };
        pu.detail_request.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        ui.global::<PlaylistDetail>().set_cloud_collection(false);
        // A close can land on a header whose rows never arrived, and the next open repaints this
        // anyway — leaving it set would make the grid's next owner show the loading line.
        ui.global::<PlaylistDetail>().set_loading(false);
        track_detail::forget_closed(&ui, &s, &*pu, &ui.global::<PlaylistDetail>());

        let pu = pu.clone();
        s.runtime.spawn_blocking(move || {
            pu.release_detail_artwork();
            pu.prewarm_visible_covers();
        });
    });
}

/// A row drag released. Optimistically permutes the visible rows and the position-order cache,
/// then writes; a failed write rolls both back.
fn wire_reorder(ui: &AppWindow, state: &AppState, playlists_ui: &Arc<PlaylistsUi>) {
    let s = state.clone();
    let pu = playlists_ui.clone();
    let weak = ui.as_weak();
    ui.global::<PlaylistDetail>().on_reorder(move |from, to| {
        let Some(ui) = weak.upgrade() else { return };
        let Ok(from_u) = usize::try_from(from) else {
            return;
        };
        let Ok(to_u) = usize::try_from(to) else {
            return;
        };
        if from_u == to_u {
            return;
        }
        let playlist_id = pu.detail_playlist_id();
        if playlist_id < 0 {
            return;
        }
        let Some(previous_order) =
            playlists_ui_mod::apply_optimistic_reorder(&ui, &pu, from_u, to_u)
        else {
            return;
        };
        let s = s.clone();
        let pu = pu.clone();
        let weak = weak.clone();
        s.runtime.clone().spawn(async move {
            if let Err(e) = library::playlists::reorder_playlist(&s.db, playlist_id, from, to).await
            {
                log::warn!("playlists::reorder: {}", describe(&e));
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    playlists_ui_mod::rollback_reorder(&ui, &pu, previous_order);
                });
            }
        });
    });
}

/// Opens the Edit Artwork picker seeding `current-artwork` from `PlaylistDetail.cover`, already
/// decoded for the open detail, so the dialog opens on a preview of the saved state.
///
/// Rename and Delete need no handler here: their opens are populated inline in
/// `my-library/tab-pills.slint`, and their commits are `super::crud`'s.
fn wire_edit_artwork(ui: &AppWindow, state: &AppState, playlists_ui: &Arc<PlaylistsUi>) {
    let s = state.clone();
    let pu = playlists_ui.clone();
    let weak = ui.as_weak();
    ui.global::<PlaylistDetail>().on_request_edit_artwork(move || {
        let id = pu.detail_playlist_id();
        if id < 0 {
            return;
        }
        let Some(claim) = DialogClaim::take_from(&weak) else { return };
        open_edit_artwork(&s, &weak, claim, id, |ui| ui.global::<PlaylistDetail>().get_cover());
    });
}

/// The row menu's removal. One row and a whole selection both arrive as `[int]` and collapse to
/// one batched write; `refresh_detail` prunes the removed ids out of the selection on the way
/// back.
fn wire_remove_track(ui: &AppWindow, state: &AppState, playlists_ui: &Arc<PlaylistsUi>) {
    let s = state.clone();
    let pu = playlists_ui.clone();
    let weak = ui.as_weak();
    ui.global::<PlaylistDetail>().on_remove_track(move |track_ids| {
        let id_vec = collect_track_ids(&track_ids);
        let playlist_id = pu.detail_playlist_id();
        if playlist_id < 0 || id_vec.is_empty() {
            return;
        }
        let s = s.clone();
        let pu = pu.clone();
        let weak = weak.clone();
        s.runtime.clone().spawn(async move {
            if let Err(e) =
                library::playlists::remove_tracks_from_playlist_batch(&s.db, playlist_id, id_vec)
                    .await
            {
                log::warn!("playlists::remove_track: {}", describe(&e));
                return;
            }
            if let Err(e) =
                playlists_ui_mod::refresh_detail(&s, &pu, weak.clone(), playlist_id).await
            {
                log::warn!("playlists::remove_track refresh: {}", describe(&e));
            }
            if let Err(e) = playlists_ui_mod::fetch_grid(&s, &pu, weak).await {
                log::warn!("playlists::remove_track refetch grid: {}", describe(&e));
            }
        });
    });
}
