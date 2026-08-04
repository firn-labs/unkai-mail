/**
 * Message crypto: decryption plus PGP key and S/MIME certificate
 * management. Private-key material never reaches the UI; these
 * commands operate on backend keychain entries by reference.
 *
 * Generated wrappers over the backend commands (#473) — one typed
 * function per `#[tauri::command]`. Argument keys mirror the Rust
 * parameter names (camelCased, as Tauri expects on the wire).
 */

import { call } from './core'
import type {
  Email,
  PgpKeyStatus,
  PgpPublicKeyDto,
  SmimeCertDto,
  SmimeCertStatus,
} from './types'

export function decryptMessage(args: {
  accountId: string
  folder: string
  uid: number
  pgpPassphrase: string
}): Promise<Email> {
  return call('decrypt_message', args)
}

export function tryAutoDecryptMessage(args: {
  accountId: string
  folder: string
  uid: number
}): Promise<Email | null> {
  return call('try_auto_decrypt_message', args)
}

export function downloadDecryptedAttachment(args: {
  accountId: string
  folder: string
  uid: number
  partId: number
  pgpPassphrase: string
}): Promise<number[]> {
  return call('download_decrypted_attachment', args)
}

export function pgpDisableUnlockAutomatically(args: { accountId: string }): Promise<void> {
  return call('pgp_disable_unlock_automatically', args)
}

export function pgpEnableUnlockAutomatically(args: {
  accountId: string
  passphrase: string
}): Promise<void> {
  return call('pgp_enable_unlock_automatically', args)
}

export function pgpGetAccountKeyStatus(args: { accountId: string }): Promise<PgpKeyStatus> {
  return call('pgp_get_account_key_status', args)
}

export function pgpGetKeysForEmail(args: { email: string }): Promise<PgpPublicKeyDto[]> {
  return call('pgp_get_keys_for_email', args)
}

export function pgpHasUnlockAutomatically(args: { accountId: string }): Promise<boolean> {
  return call('pgp_has_unlock_automatically', args)
}

export function pgpImportPrivateKey(args: {
  accountId: string
  armoredKey: string
  passphrase: string
}): Promise<string> {
  return call('pgp_import_private_key', args)
}

export function pgpImportPublicKey(args: {
  armoredKey: string
  emailHint?: string | null
}): Promise<string> {
  return call('pgp_import_public_key', args)
}

export function pgpListPublicKeys(): Promise<PgpPublicKeyDto[]> {
  return call('pgp_list_public_keys')
}

export function pgpRemovePrivateKey(args: { accountId: string }): Promise<void> {
  return call('pgp_remove_private_key', args)
}

export function pgpRemovePublicKey(args: { fingerprint: string }): Promise<void> {
  return call('pgp_remove_public_key', args)
}

export function smimeDisableUnlockAutomatically(args: { accountId: string }): Promise<void> {
  return call('smime_disable_unlock_automatically', args)
}

export function smimeEnableUnlockAutomatically(args: {
  accountId: string
  passphrase: string
}): Promise<void> {
  return call('smime_enable_unlock_automatically', args)
}

export function smimeGetAccountCertStatus(args: { accountId: string }): Promise<SmimeCertStatus> {
  return call('smime_get_account_cert_status', args)
}

export function smimeGetCertsForEmail(args: { email: string }): Promise<SmimeCertDto[]> {
  return call('smime_get_certs_for_email', args)
}

export function smimeHasUnlockAutomatically(args: { accountId: string }): Promise<boolean> {
  return call('smime_has_unlock_automatically', args)
}

export function smimeImportPkcs12(args: {
  accountId: string
  pkcs12Base64: string
  passphrase: string
}): Promise<string> {
  return call('smime_import_pkcs12', args)
}

export function smimeImportPublicCert(args: {
  certData: string
  emailHint?: string | null
}): Promise<string> {
  return call('smime_import_public_cert', args)
}

export function smimeListPublicCerts(): Promise<SmimeCertDto[]> {
  return call('smime_list_public_certs')
}

export function smimeRemovePrivateCert(args: { accountId: string }): Promise<void> {
  return call('smime_remove_private_cert', args)
}

export function smimeRemovePublicCert(args: { fingerprint: string }): Promise<void> {
  return call('smime_remove_public_cert', args)
}
