/**
 * Nextcloud Talk rooms and participants.
 *
 * Generated wrappers over the backend commands (#473) — one typed
 * function per `#[tauri::command]`. Argument keys mirror the Rust
 * parameter names (camelCased, as Tauri expects on the wire).
 */

import { call } from './core'
import type {
  ParticipantSource,
  TalkRoom,
} from './types'

export function listTalkRooms(args: { ncId: string }): Promise<TalkRoom[]> {
  return call('list_talk_rooms', args)
}

export function createTalkRoom(args: {
  ncId: string
  roomName: string
  participants: ParticipantSource[]
  objectType?: string | null
  objectId?: string | null
  roomType?: number | null
}): Promise<TalkRoom> {
  return call('create_talk_room', args)
}

export function deleteTalkRoom(args: { ncId: string; roomToken: string }): Promise<void> {
  return call('delete_talk_room', args)
}

export function renameTalkRoom(args: {
  ncId: string
  roomToken: string
  newName: string
}): Promise<void> {
  return call('rename_talk_room', args)
}

export function addTalkParticipant(args: {
  ncId: string
  roomToken: string
  participant: ParticipantSource
}): Promise<void> {
  return call('add_talk_participant', args)
}

export function addTalkParticipants(args: {
  ncId: string
  roomToken: string
  participants: ParticipantSource[]
}): Promise<void> {
  return call('add_talk_participants', args)
}

export function setTalkRoomPublic(args: {
  ncId: string
  roomToken: string
  public: boolean
}): Promise<void> {
  return call('set_talk_room_public', args)
}
