// Copyright 2026 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use cosmic_settings_audio_client::{self as audio_client};

pub type NodeId = u32;
pub type DeviceId = u32;

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
    /// Device the default sink/source last sat on. Kept here rather than read back
    /// out of `Nodes` because a profile switch removes the old node before the new
    /// default is announced, so by then there is nothing left to compare against.
    sink_default_device: Option<DeviceId>,
    source_default_device: Option<DeviceId>,
}

#[derive(Debug, Default)]
pub struct Nodes {
    active: Option<usize>,
    mute: Vec<bool>,
    id: Vec<NodeId>,
    /// Which device the node hangs off, when it reports one.
    device_id: Vec<Option<DeviceId>>,
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
        self.device_id.remove(pos);
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
        self.device_id.push(None);
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

/// Whether a default-node reassignment stayed on the same piece of hardware.
///
/// A Bluetooth headset has to leave A2DP for HFP to offer a microphone, so the
/// moment anything opens capture the sink node is destroyed and rebuilt under
/// the other profile — at that profile's own volume — and rebuilt again on the
/// way back. Each rebuild reaches us as a `DefaultSink` for a node we have never
/// seen, which is indistinguishable from a device switch except that the new
/// node hangs off the same device. Nobody asked for a volume change, so nothing
/// should be shown for one; picking a genuinely different output still does.
fn same_device(previous: Option<DeviceId>, current: Option<DeviceId>) -> bool {
    previous.is_some() && previous == current
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
                    let device = self.sinks.device_id[pos];
                    let previous_device = std::mem::replace(&mut self.sink_default_device, device);
                    let baseline = !std::mem::replace(&mut self.sink_default_seen, true);
                    let (volume, mute) = (self.active_sink.volume, self.active_sink.mute);
                    return (!baseline && !same_device(previous_device, device))
                        .then_some(Response::SinkVolume(volume, mute));
                }
            }

            audio_client::Event::DefaultSource(node_id) => {
                self.default_source = Some(node_id);
                if let Some(pos) = self.sources.id.iter().position(|&id| id == node_id) {
                    self.sources.active = Some(pos);
                    self.active_source.mute = self.sources.mute[pos];
                    self.active_source.volume = self.sources.volume[pos];
                    let device = self.sources.device_id[pos];
                    let previous_device =
                        std::mem::replace(&mut self.source_default_device, device);
                    let baseline = !std::mem::replace(&mut self.source_default_seen, true);
                    let (volume, mute) = (self.active_source.volume, self.active_source.mute);
                    return (!baseline && !same_device(previous_device, device))
                        .then_some(Response::SourceVolume(volume, mute));
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
                    self.sinks.device_id[pos] = node.device_id;

                    if let Some(default_node_id) = self.default_sink
                        && default_node_id == node_id
                    {
                        self.sinks.active = Some(pos);
                        self.active_sink.mute = self.sinks.mute[pos];
                        self.active_sink.volume = self.sinks.volume[pos];
                        self.sink_default_device = node.device_id;
                    }
                } else {
                    let pos = self
                        .sources
                        .id
                        .iter()
                        .position(|&id| id == node_id)
                        .unwrap_or_else(|| self.sources.push(node_id));
                    self.sources.device_id[pos] = node.device_id;

                    if let Some(default_node_id) = self.default_source
                        && default_node_id == node_id
                    {
                        self.sources.active = Some(pos);
                        self.active_source.mute = self.sources.mute[pos];
                        self.active_source.volume = self.sources.volume[pos];
                        self.source_default_device = node.device_id;
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

#[cfg(test)]
mod tests {
    use super::*;
    use audio_client::{Event, NodeInfo};

    fn sink(device_id: Option<DeviceId>) -> NodeInfo {
        NodeInfo {
            name: String::new(),
            description: String::new(),
            device_profile_description: String::new(),
            device_id,
            card_profile_device: None,
            is_sink: true,
        }
    }

    /// Bring a model up in the order the daemon replays state to a new client:
    /// nodes, the default assignment, then each node's volume and mute.
    fn connected(device_id: Option<DeviceId>, volume: u32) -> (Model, NodeId) {
        let mut model = Model::default();
        let node_id = 10;
        assert!(
            model
                .update(Event::Node(node_id, sink(device_id)))
                .is_none()
        );
        assert!(model.update(Event::DefaultSink(node_id)).is_none());
        assert!(
            model
                .update(Event::NodeVolume(node_id, volume, None))
                .is_none()
        );
        assert!(model.update(Event::NodeMute(node_id, false)).is_none());
        (model, node_id)
    }

    #[test]
    fn startup_state_shows_nothing() {
        let (model, _) = connected(Some(1), 62);
        assert_eq!(model.active_sink.volume, 62);
    }

    /// A2DP -> HFP: the sink is rebuilt as a new node on the same device, at that
    /// profile's own volume. The user asked for voice input, not a volume change.
    #[test]
    fn profile_switch_shows_nothing() {
        let (mut model, _) = connected(Some(1), 62);

        model.update(Event::RemoveNode(10));
        assert!(model.update(Event::Node(11, sink(Some(1)))).is_none());
        assert!(model.update(Event::DefaultSink(11)).is_none());
        assert!(model.update(Event::NodeVolume(11, 90, None)).is_none());

        // ...and back again when capture ends.
        model.update(Event::RemoveNode(11));
        assert!(model.update(Event::Node(12, sink(Some(1)))).is_none());
        assert!(model.update(Event::DefaultSink(12)).is_none());
        assert!(model.update(Event::NodeVolume(12, 62, None)).is_none());
    }

    /// The same rebuild, with the default announced before the node it names.
    #[test]
    fn profile_switch_shows_nothing_when_default_precedes_node() {
        let (mut model, _) = connected(Some(1), 62);

        model.update(Event::RemoveNode(10));
        assert!(model.update(Event::DefaultSink(11)).is_none());
        assert!(model.update(Event::Node(11, sink(Some(1)))).is_none());
        assert!(model.update(Event::NodeVolume(11, 90, None)).is_none());

        // The tracker still knows the device, so the switch back is quiet too.
        model.update(Event::RemoveNode(11));
        assert!(model.update(Event::Node(12, sink(Some(1)))).is_none());
        assert!(model.update(Event::DefaultSink(12)).is_none());
    }

    #[test]
    fn switching_to_another_device_still_shows() {
        let (mut model, _) = connected(Some(1), 62);

        assert!(model.update(Event::Node(11, sink(Some(2)))).is_none());
        assert!(model.update(Event::NodeVolume(11, 40, None)).is_none());
        assert!(matches!(
            model.update(Event::DefaultSink(11)),
            Some(Response::SinkVolume(40, false))
        ));
    }

    /// Nodes that report no device can't be told apart, so they are treated as
    /// separate hardware rather than silently suppressed.
    #[test]
    fn deviceless_nodes_still_show() {
        let (mut model, _) = connected(None, 62);

        assert!(model.update(Event::Node(11, sink(None))).is_none());
        assert!(model.update(Event::NodeVolume(11, 40, None)).is_none());
        assert!(matches!(
            model.update(Event::DefaultSink(11)),
            Some(Response::SinkVolume(40, false))
        ));
    }

    #[test]
    fn volume_and_mute_changes_still_show() {
        let (mut model, node_id) = connected(Some(1), 62);

        assert!(matches!(
            model.update(Event::NodeVolume(node_id, 70, None)),
            Some(Response::SinkVolume(70, false))
        ));
        assert!(matches!(
            model.update(Event::NodeMute(node_id, true)),
            Some(Response::SinkVolume(70, true))
        ));
    }
}
