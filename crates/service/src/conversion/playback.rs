use super::{
    Arc, BTreeMap, Bytes, HashMap, JobId, PLAYBACK_HUBS, RANGE_CHUNK_BYTES, StdMutex, Uuid,
    broadcast,
};

#[derive(Clone, Debug)]
pub enum PlaybackPacket {
    Audio(Bytes),
    Reset,
}

#[derive(Debug, Default)]
pub(super) struct PlaybackSegmentBuffer {
    pub(super) chunks: Vec<Bytes>,
    pub(super) complete: bool,
    pub(super) published: bool,
}

#[derive(Debug, Default)]
pub(super) struct PlaybackOrderState {
    pub(super) next_ordinal: usize,
    pub(super) segments: BTreeMap<usize, PlaybackSegmentBuffer>,
}

#[derive(Debug)]
pub(super) struct PlaybackHub {
    pub(super) sender: broadcast::Sender<PlaybackPacket>,
    pub(super) order: StdMutex<PlaybackOrderState>,
}

pub(super) fn playback_hub(job_id: JobId) -> Arc<PlaybackHub> {
    let registry = PLAYBACK_HUBS.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut registry = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    registry
        .entry(job_id.as_uuid())
        .or_insert_with(|| {
            Arc::new(PlaybackHub {
                sender: broadcast::channel(64).0,
                order: StdMutex::new(PlaybackOrderState::default()),
            })
        })
        .clone()
}

pub fn subscribe_playback(job_id: Uuid) -> broadcast::Receiver<PlaybackPacket> {
    playback_hub(JobId::from_uuid(job_id)).sender.subscribe()
}

pub(super) fn playback_listener_count(job_id: JobId) -> usize {
    playback_hub(job_id).sender.receiver_count()
}

pub(super) fn prepare_playback(job_id: JobId, next_ordinal: usize) {
    let hub = playback_hub(job_id);
    let mut order = hub
        .order
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *order = PlaybackOrderState {
        next_ordinal,
        segments: BTreeMap::new(),
    };
}

pub(super) fn publish_playback_chunk(job_id: JobId, ordinal: usize, pcm: Bytes) {
    if pcm.is_empty() {
        return;
    }
    let hub = playback_hub(job_id);
    if hub.sender.receiver_count() == 0 {
        return;
    }
    let mut order = hub
        .order
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if ordinal < order.next_ordinal {
        return;
    }
    order.segments.entry(ordinal).or_default().chunks.push(pcm);
    drain_playback_order(&hub, &mut order);
}

pub(super) fn complete_playback_with_pcm(job_id: JobId, ordinal: usize, pcm: &[u8]) {
    for chunk in pcm.chunks(RANGE_CHUNK_BYTES - (RANGE_CHUNK_BYTES % 4)) {
        publish_playback_chunk(job_id, ordinal, Bytes::copy_from_slice(chunk));
    }
    complete_playback_segment(job_id, ordinal);
}

pub(super) fn complete_playback_segment(job_id: JobId, ordinal: usize) {
    let hub = playback_hub(job_id);
    let mut order = hub
        .order
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if ordinal < order.next_ordinal {
        return;
    }
    order.segments.entry(ordinal).or_default().complete = true;
    drain_playback_order(&hub, &mut order);
}

pub(super) fn reset_playback_segment(job_id: JobId, ordinal: usize) {
    let hub = playback_hub(job_id);
    let mut order = hub
        .order
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if ordinal < order.next_ordinal {
        return;
    }
    let segment = order.segments.entry(ordinal).or_default();
    segment.chunks.clear();
    segment.complete = false;
    if segment.published {
        segment.published = false;
        let _ = hub.sender.send(PlaybackPacket::Reset);
    }
}

pub(super) fn drain_playback_order(hub: &PlaybackHub, order: &mut PlaybackOrderState) {
    loop {
        let next = order.next_ordinal;
        let Some(segment) = order.segments.get_mut(&next) else {
            break;
        };
        for chunk in segment.chunks.drain(..) {
            if hub.sender.send(PlaybackPacket::Audio(chunk)).is_ok() {
                segment.published = true;
            }
        }
        if !segment.complete {
            break;
        }
        order.segments.remove(&next);
        order.next_ordinal = order.next_ordinal.saturating_add(1);
    }
}
