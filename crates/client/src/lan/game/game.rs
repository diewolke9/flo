use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::watch::Receiver as WatchReceiver;

use flo_w3gs::net::W3GSStream;
use flo_w3gs::packet::*;
use flo_w3gs::protocol::action::{IncomingAction, OutgoingAction, OutgoingKeepAlive};
use flo_w3gs::protocol::chat::{ChatMessage, ChatToHost};
use flo_w3gs::protocol::leave::LeaveAck;

use crate::error::*;
use crate::lan::game::LanGameInfo;
use crate::node::stream::NodeStreamHandle;
use crate::node::NodeInfo;
use crate::types::{NodeGameStatus, SlotClientStatus};
use flo_util::chat::parse_chat_command;
use flo_w3gs::chat::ChatFromHost;
use std::collections::BTreeSet;

#[derive(Debug)]
pub enum GameResult {
  Disconnected,
  Leave,
}

#[derive(Debug)]
pub struct GameHandler<'a> {
  info: &'a LanGameInfo,
  node: &'a NodeInfo,
  w3gs_stream: &'a mut W3GSStream,
  node_stream: &'a mut NodeStreamHandle,
  status_rx: &'a mut WatchReceiver<Option<NodeGameStatus>>,
  w3gs_tx: &'a mut Sender<Packet>,
  w3gs_rx: &'a mut Receiver<Packet>,
  tick_recv: u32,
  tick_ack: u32,
  muted_players: BTreeSet<u8>,
}

impl<'a> GameHandler<'a> {
  pub fn new(
    info: &'a LanGameInfo,
    node: &'a NodeInfo,
    stream: &'a mut W3GSStream,
    node_stream: &'a mut NodeStreamHandle,
    status_rx: &'a mut WatchReceiver<Option<NodeGameStatus>>,
    w3gs_tx: &'a mut Sender<Packet>,
    w3gs_rx: &'a mut Receiver<Packet>,
  ) -> Self {
    GameHandler {
      info,
      node,
      w3gs_stream: stream,
      node_stream,
      status_rx,
      w3gs_tx,
      w3gs_rx,
      tick_recv: 0,
      tick_ack: 0,
      muted_players: BTreeSet::new(),
    }
  }

  pub async fn run(&mut self) -> Result<GameResult> {
    let mut loop_state = GameLoopState::new(&self.info);

    loop {
      tokio::select! {
        next = self.w3gs_stream.recv() => {
          let pkt = match next {
            Ok(pkt) => pkt,
            Err(err) => {
              tracing::error!("game connection: {}", err);
              return Ok(GameResult::Disconnected)
            },
          };
          if let Some(pkt) = pkt {
            // tracing::debug!("game => {:?}", pkt.type_id());
            if pkt.type_id() == LeaveAck::PACKET_TYPE_ID {
              self.node_stream.report_slot_status(SlotClientStatus::Left).await.ok();
              self.w3gs_stream.send(Packet::simple(LeaveAck)?).await?;
              self.w3gs_stream.flush().await?;
              return Ok(GameResult::Leave)
            }

            self.handle_game_packet(&mut loop_state, pkt).await?;
          } else {
            tracing::error!("stream closed");
            return Ok(GameResult::Disconnected)
          }
        }
        next = self.status_rx.recv() => {
          let next = if let Some(next) = next {
            next
          } else {
            return Err(Error::TaskCancelled(anyhow::format_err!("game status tx dropped")))
          };
          match next {
            Some(status) => {
              self.handle_game_status_change(&mut loop_state, status).await?;
            },
            None => {},
          }
        }
        next = self.w3gs_rx.recv() => {
          if let Some(pkt) = next {
            self.handle_incoming_w3gs(&mut loop_state, pkt).await?;
          } else {
            return Err(Error::TaskCancelled(anyhow::format_err!("w3g tx dropped")))
          }
        }
      }
    }
  }

  #[inline]
  async fn handle_incoming_w3gs(&mut self, _state: &mut GameLoopState, pkt: Packet) -> Result<()> {
    match pkt.type_id() {
      OutgoingKeepAlive::PACKET_TYPE_ID => {}
      IncomingAction::PACKET_TYPE_ID => {
        self.tick_recv += 1;
      }
      OutgoingAction::PACKET_TYPE_ID => {}
      ChatFromHost::PACKET_TYPE_ID => {
        if !self.muted_players.is_empty() {
          let pkt: ChatFromHost = pkt.decode_simple()?;
          if let ChatToHost {
            message: ChatMessage::Scoped { .. },
            ..
          } = pkt.0
          {
            if self.muted_players.contains(&pkt.from_player()) {
              return Ok(());
            }
          }
        }
      }
      _ => {}
    }

    self.w3gs_stream.send(pkt).await?;
    Ok(())
  }

  async fn handle_game_status_change(
    &mut self,
    _state: &mut GameLoopState,
    status: NodeGameStatus,
  ) -> Result<()> {
    tracing::debug!("game status changed: {:?}", status);
    Ok(())
  }

  async fn handle_game_packet(&mut self, _state: &mut GameLoopState, pkt: Packet) -> Result<()> {
    match pkt.type_id() {
      ChatToHost::PACKET_TYPE_ID => {
        let pkt: ChatToHost = pkt.decode_simple()?;
        match pkt.message {
          ChatMessage::Scoped { message, .. } => {
            if let Some(cmd) = parse_chat_command(message.as_bytes()) {
              self.handle_chat_command(&cmd);
              return Ok(());
            }
          }
          _ => {}
        }
      }
      OutgoingKeepAlive::PACKET_TYPE_ID => self.tick_ack += 1,
      IncomingAction::PACKET_TYPE_ID => {}
      OutgoingAction::PACKET_TYPE_ID => {}
      _ => {
        tracing::debug!("unknown game packet: {:?}", pkt.type_id());
      }
    }

    self.node_stream.send_w3gs(pkt).await?;

    Ok(())
  }

  fn handle_chat_command(&mut self, cmd: &str) {
    match cmd.trim_end() {
      "help" => {
        let messages = vec![
          "Chat commands:".to_string(),
          " !flo: print game information.".to_string(),
          " !muteall: Mute all players.".to_string(),
          " !unmuteall: Mute all players.".to_string(),
          " !mute: Mute your opponent (1v1), or display a player list.".to_string(),
          " !mute <ID>: Mute a player.".to_string(),
          " !unmute: Unmute your opponent (1v1), or display a player list.".to_string(),
          " !unmute <ID>: Unmute a player.".to_string(),
        ];
        self.send_chats_to_self(self.info.slot_info.slot_player_id, messages)
      }
      "flo" => {
        let mut messages = vec![
          format!(
            "Game: {} (#{})",
            self.info.game.name, self.info.game.game_id
          ),
          format!(
            "Server: {}, {}, {} (#{})",
            self.node.name, self.node.location, self.node.country_id, self.node.id
          ),
          "Players:".to_string(),
        ];

        for slot in &self.info.game.slots {
          if let Some(ref player) = slot.player.as_ref() {
            messages.push(format!(
              "  {}: Team {}, {:?}",
              player.name, slot.settings.team, slot.settings.race
            ));
          }
        }

        self.send_chats_to_self(self.info.slot_info.slot_player_id, messages)
      }
      "tick" => self.send_chats_to_self(
        self.info.slot_info.slot_player_id,
        vec![format!(
          "tick_recv = {}, tick_ack = {}",
          self.tick_recv, self.tick_ack
        )],
      ),
      "muteall" => {
        let targets: Vec<u8> = self
          .info
          .slot_info
          .player_infos
          .iter()
          .filter_map(|slot| {
            if slot.slot_player_id == self.info.slot_info.slot_player_id {
              return None;
            }
            Some(slot.slot_player_id)
          })
          .collect();
        self.muted_players.extend(targets);
        self.send_chats_to_self(
          self.info.slot_info.slot_player_id,
          vec![format!("All players muted.")],
        );
      }
      "unmuteall" => {
        self.muted_players.clear();
        self.send_chats_to_self(
          self.info.slot_info.slot_player_id,
          vec![format!("All players un-muted.")],
        );
      }
      cmd if cmd.starts_with("mute") => {
        let targets: Vec<(u8, &str)> = self
          .info
          .slot_info
          .player_infos
          .iter()
          .filter_map(|slot| {
            if slot.slot_player_id == self.info.slot_info.slot_player_id {
              return None;
            }
            if !self.muted_players.contains(&slot.slot_player_id) {
              Some((slot.slot_player_id, slot.name.as_str()))
            } else {
              None
            }
          })
          .collect();

        if cmd.trim_end() == "mute" {
          match targets.len() {
            0 => {
              self.send_chats_to_self(
                self.info.slot_info.slot_player_id,
                vec![format!("You have silenced all the players.")],
              );
              return;
            }
            1 => {
              self.muted_players.insert(targets[0].0);
              self.send_chats_to_self(
                self.info.slot_info.slot_player_id,
                vec![format!("Muted: {}", targets[0].1)],
              );
            }
            _ => {
              let mut msgs = vec![format!("Type `!mute <ID>` to mute a player:")];
              for (id, name) in targets {
                msgs.push(format!(" ID={} {}", id, name));
              }
              self.send_chats_to_self(self.info.slot_info.slot_player_id, msgs);
            }
          }
        } else {
          if let Some(id) = (&cmd["mute ".len()..]).parse::<u8>().ok() {
            if let Some(info) = self
              .info
              .slot_info
              .player_infos
              .iter()
              .find(|info| info.slot_player_id == id)
            {
              self.muted_players.insert(id);
              self.send_chats_to_self(
                self.info.slot_info.slot_player_id,
                vec![format!("Muted: {}", info.name)],
              );
            } else {
              self.send_chats_to_self(self.info.slot_info.slot_player_id, {
                let mut msgs = vec![format!("Invalid player id. Players:")];
                for (id, name) in targets {
                  msgs.push(format!(" ID={} {}", id, name));
                }
                msgs
              });
            }
          } else {
            self.send_chats_to_self(
              self.info.slot_info.slot_player_id,
              vec![format!("Invalid syntax. Example: !mute 1")],
            );
          }
        }
      }
      cmd if cmd.starts_with("unmute") => {
        let targets: Vec<(u8, &str)> = self
          .muted_players
          .iter()
          .cloned()
          .filter_map(|id| {
            if id == self.info.slot_info.slot_player_id {
              return None;
            }
            self
              .info
              .slot_info
              .player_infos
              .iter()
              .find(|info| info.slot_player_id == id)
              .map(|info| (info.slot_player_id, info.name.as_str()))
          })
          .collect();

        if cmd.trim_end() == "unmute" {
          match targets.len() {
            0 => {
              self.send_chats_to_self(
                self.info.slot_info.slot_player_id,
                vec![format!("No player to unmute.")],
              );
              return;
            }
            1 => {
              self.muted_players.remove(&targets[0].0);
              self.send_chats_to_self(
                self.info.slot_info.slot_player_id,
                vec![format!("Un-muted: {}", targets[0].1)],
              );
            }
            _ => {
              let mut msgs = vec![format!("Type `!unmute <ID>` to unmute a player:")];
              for (id, name) in targets {
                msgs.push(format!(" ID={} {}", id, name));
              }
              self.send_chats_to_self(self.info.slot_info.slot_player_id, msgs);
            }
          }
        } else {
          if let Some(id) = (&cmd["unmute ".len()..]).parse::<u8>().ok() {
            if let Some(name) = targets.iter().find(|info| info.0 == id).map(|info| info.1) {
              self.muted_players.remove(&id);
              self.send_chats_to_self(
                self.info.slot_info.slot_player_id,
                vec![format!("Un-muted: {}", name)],
              );
            } else {
              self.send_chats_to_self(self.info.slot_info.slot_player_id, {
                let mut msgs = vec![format!("Invalid player id. Muted players:")];
                for (id, name) in targets {
                  msgs.push(format!(" ID={} {}", id, name));
                }
                msgs
              });
            }
          } else {
            self.send_chats_to_self(
              self.info.slot_info.slot_player_id,
              vec![format!("Invalid syntax. Example: !unmute 1")],
            );
          }
        }
      }
      _ => self.send_chats_to_self(
        self.info.slot_info.slot_player_id,
        vec![format!("Unknown command")],
      ),
    }
  }

  fn send_chats_to_self(&self, player_id: u8, messages: Vec<String>) {
    let mut tx = self.w3gs_tx.clone();
    tokio::spawn(async move {
      for message in messages {
        match Packet::simple(ChatFromHost::private_to_self(player_id, message)) {
          Ok(pkt) => {
            tx.send(pkt).await.ok();
          }
          Err(err) => {
            tracing::error!("encode chat packet: {}", err);
          }
        }
      }
    });
  }
}

#[derive(Debug)]
struct GameLoopState {
  time: u32,
  ping: Option<u32>,
}

impl GameLoopState {
  fn new(_info: &LanGameInfo) -> Self {
    GameLoopState {
      time: 0,
      ping: None,
    }
  }
}
