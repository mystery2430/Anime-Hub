/**
 * AniList ayar yaması üretimi.
 *
 * Alt alanlar boş gönderildiğinde backend kayıtlı değeri korur; alan her
 * açılışta yine boş yüklenir, dolayısıyla dokunulmamış bir anahtarın
 * yeniden gönderilmesi boş bir değer yollamak olur. Bu yüzden sadece
 * kullanıcının gerçekten dokunduğu anahtarlar gönderilir.
 */

/**
 * Build the settings patch sub-object for touched AniList fields.
 *
 * `touched` empty → returns `{}`: the `anilist` key is omitted entirely and
 * the backend keeps every stored value. Secrets are value-preserved (never
 * trimmed); identity fields (clientId, redirectUri) are trimmed.
 */
export function buildAnilistPatch(fields, touched) {
  if (!(touched instanceof Set) || touched.size === 0) {
    return {};
  }
  const anilist = {};
  for (const [key, value] of fields) {
    if (!touched.has(key)) continue;
    anilist[key] =
      key === "clientSecret" ? value ?? "" : String(value ?? "").trim();
  }
  return { anilist };
}
