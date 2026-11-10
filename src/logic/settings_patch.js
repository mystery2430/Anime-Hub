/**
 * AniList ayar yaması üretimi.
 *
 * Kayıt formu her açılışta boş yüklenmez: client id/redirect kayıtlı değerle
 * doludur, secret boştur. Backend yamayı TÜM nesne olarak alır (eksik
 * zorunlu alan red emer), dolayısıyla kapı şudur: hiçbir alana
 * dokunulmadıysa `anilist` anahtarı hiç gönderilmez (anlamsız no-op kayıt
 * olmaz); en az bir alana dokunulduysa üç alan birlikte gönderilir —
 * dokunulmamışlar kendi önceden-load edilmiş değerlerini taşır, boş secret
 * backend'de kayıtlı değeri korur (M-11).
 */

/**
 * Build the AniList sub-object of the settings patch.
 *
 * `touched` empty → `{}`: the `anilist` key is omitted entirely. Any touch →
 * all three entries are sent; secrets stay value-preserved (never trimmed),
 * identity fields are trimmed.
 */
export function buildAnilistPatch(fields, touched) {
  if (!(touched instanceof Set) || touched.size === 0) {
    return {};
  }
  const anilist = {};
  for (const [key, value] of fields) {
    anilist[key] =
      key === "clientSecret" ? value ?? "" : String(value ?? "").trim();
  }
  return { anilist };
}
