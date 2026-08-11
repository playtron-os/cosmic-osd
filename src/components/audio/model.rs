// Copyright 2026 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use cosmic_settings_audio_client::{self as audio_client};

pub type NodeId = u32;

#[derive(Debug, Default)]
pub struct Model {
    sinks: Nodes,
    sources: Nodes,
    pub active_sink: ActiveNode,
    pub active_source: ActiveNode,
    default_sink: Option<NodeId>,
    default_source: Option<NodeId>,
    /// The first default-node assignment is the state we connected to, not a change.
    sink_default_seen: bool,
    source_default_seen: bool,
}

#[derive(Debug, Default)]
pub struct Nodes {
    active: Option<usize>,
    mute: Vec<bool>,
    id: Vec<NodeId>,
    volume: Vec<u32>,
    /// `Event::Node` carries no volume/mute (see `NodeInfo`), so a node is seeded with
    /// placeholders and its real values arrive as separate events. Those first reports
    /// are state, not user action — these track which have landed, per node.
    volume_seen: Vec<bool>,
    mute_seen: Vec<bool>,
}

impl Nodes {
    pub fn remove(&mut self, node_id: u32) -> bool {
        let Some(pos) = self.id.iter().position(|id| node_id == *id) else {
            return false;
        };
        self.mute.remove(pos);
        self.id.remove(pos);
        self.volume.remove(pos);
        self.volume_seen.remove(pos);
        self.mute_seen.remove(pos);
        if self.active == Some(pos) {
            self.active = None;
        }
        true
    }

    fn push(&mut self, node_id: NodeId) -> usize {
        self.id.push(node_id);
        self.volume.push(0);
        self.mute.push(false);
        self.volume_seen.push(false);
        self.mute_seen.push(false);
        self.id.len() - 1
    }
}

#[derive(Debug, Default)]
pub struct ActiveNode {
    pub volume: u32,
    pub mute: bool,
}

pub enum Response {
    SinkVolume(u32, bool),
    SourceVolume(u32, bool),
}

impl Model {
    pub fn update(&mut self, event: audio_client::Event) -> Option<Response> {
        match event {
            audio_client::Event::NodeMute(node_id, mute) => {
                if let Some(pos) = self.sinks.id.iter().position(|id| node_id == *id) {
                    self.sinks.mute[pos] = mute;
                    let baseline = !std::mem::replace(&mut self.sinks.mute_seen[pos], true);
                    if self.sinks.active == Some(pos) && self.active_sink.mute != mute {
                        self.active_sink.mute = mute;
                        let volume = self.sinks.volume[pos];
                        return (!baseline).then_some(Response::SinkVolume(volume, mute));
                    }
                } else if let Some(pos) = self.sources.id.iter().position(|id| node_id == *id) {
                    self.sources.mute[pos] = mute;
                    let baseline = !std::mem::replace(&mut self.sources.mute_seen[pos], true);
                    if self.sources.active == Some(pos) && self.active_source.mute != mute {
                        self.active_source.mute = mute;
                        let volume = self.sources.volume[pos];
                        return (!baseline).then_some(Response::SourceVolume(volume, mute));
                    }
                }
            }

            audio_client::Event::NodeVolume(node_id, volume, _balance) => {
                if let Some(pos) = self.sinks.id.iter().position(|id| node_id == *id) {
                    self.sinks.volume[pos] = volume;
                    let baseline = !std::mem::replace(&mut self.sinks.volume_seen[pos], true);
                    if self.default_sink.as_ref().is_some_and(|&id| id == node_id)
                        && let Some(pos) = self.sinks.active
                    {
                        let changed = self.active_sink.mute != self.sinks.mute[pos]
                            || self.active_sink.volume != self.sinks.volume[pos];
                        self.active_sink.mute = self.sinks.mute[pos];
                        self.active_sink.volume = self.sinks.volume[pos];

                        if !changed {
                            return None;
                        }
                        let (volume, mute) = (self.active_sink.volume, self.active_sink.mute);
                        return (!baseline).then_some(Response::SinkVolume(volume, mute));
                    }
                } else if let Some(pos) = self.sources.id.iter().position(|id| node_id == *id) {
                    self.sources.volume[pos] = volume;
                    let baseline = !std::mem::replace(&mut self.sources.volume_seen[pos], true);
                    if self
                        .default_source
                        .as_ref()
                        .is_some_and(|&id| id == node_id)
                        && let Some(pos) = self.sources.active
                    {
                        let changed = self.active_source.mute != self.sources.mute[pos]
                            || self.active_source.volume != self.sources.volume[pos];
                        self.active_source.mute = self.sources.mute[pos];
                        self.active_source.volume = self.sources.volume[pos];
                        if !changed {
                            return None;
                        }
                        let (volume, mute) = (self.active_source.volume, self.active_source.mute);
                        return (!baseline).then_some(Response::SourceVolume(volume, mute));
                    }
                }
            }

            audio_client::Event::DefaultSink(node_id) => {
                self.default_sink = Some(node_id);
                if let Some(pos) = self.sinks.id.iter().position(|&id| id == node_id) {
                    self.sinks.active = Some(pos);
                    self.active_sink.mute = self.sinks.mute[pos];
                    self.active_sink.volume = self.sinks.volume[pos];
                    let baseline = !std::mem::replace(&mut self.sink_default_seen, true);
                    let (volume, mute) = (self.active_sink.volume, self.active_sink.mute);
                    return (!baseline).then_some(Response::SinkVolume(volume, mute));
                }
            }

            audio_client::Event::DefaultSource(node_id) => {
                self.default_source = Some(node_id);
                if let Some(pos) = self.sources.id.iter().position(|&id| id == node_id) {
                    self.sources.active = Some(pos);
                    self.active_source.mute = self.sources.mute[pos];
                    self.active_source.volume = self.sources.volume[pos];
                    let baseline = !std::mem::replace(&mut self.source_default_seen, true);
                    let (volume, mute) = (self.active_source.volume, self.active_source.mute);
                    return (!baseline).then_some(Response::SourceVolume(volume, mute));
                }
            }

            audio_client::Event::Node(node_id, node) => {
                if node.is_sink {
                    let pos = self
                        .sinks
                        .id
                        .iter()
                        .position(|&id| id == node_id)
                        .unwrap_or_else(|| self.sinks.push(node_id));

                    if let Some(default_node_id) = self.default_sink
                        && default_node_id == node_id
                    {
                        self.sinks.active = Some(pos);
                        self.active_sink.mute = self.sinks.mute[pos];
                        self.active_sink.volume = self.sinks.volume[pos];
                    }
                } else {
                    let pos = self
                        .sources
                        .id
                        .iter()
                        .position(|&id| id == node_id)
                        .unwrap_or_else(|| self.sources.push(node_id));

                    if let Some(default_node_id) = self.default_source
                        && default_node_id == node_id
                    {
                        self.sources.active = Some(pos);
                        self.active_source.mute = self.sources.mute[pos];
                        self.active_source.volume = self.sources.volume[pos];
                    }
                }
            }

            audio_client::Event::RemoveNode(node_id) => {
                if !self.sinks.remove(node_id) {
                    self.sources.remove(node_id);
                }
            }

            _ => (),
        }

        None
    }
}
