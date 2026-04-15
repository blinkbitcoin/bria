CREATE INDEX IF NOT EXISTS bdk_script_pubkeys_keychain_kind_script_idx
ON bdk_script_pubkeys (keychain_id, keychain_kind)
INCLUDE (script);
