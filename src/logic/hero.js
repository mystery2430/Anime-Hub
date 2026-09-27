/**
 * Launcher hero bar + showcase-card görsel mantığı.
 *
 * Hepsi DOM'suz saf fonksiyonlar: sayı biçimlendirme, hero modeli ve site
 * renginden vitrin kartı gradyanı üretimi burada; DOM tarafı `main.js`de.
 */

/**
 * Sayıyı Türkçe yerelde biçimler (12.345). Geçersiz veri tire olarak döner —
 * arayüzde asla "NaN" veya rastgele string gözükmez.
 */
export function formatStat(value) {
  if (typeof value !== "number" || !Number.isFinite(value) || value < 0) {
    return "—";
  }
  return new Intl.NumberFormat("tr-TR").format(Math.floor(value));
}

/**
 * Hero barın üç istatistiği + bağlantı durumu.
 * `stats` null ise kullanıcı bağlı değil demektir: sayılar tirelenir ve
 * `loggedIn=false` ile "AniList'i bağla" çağrısı görünür olur.
 */
export function heroModel(stats) {
  if (!stats || typeof stats !== "object") {
    return {
      episodes: "—",
      current: "—",
      completed: "—",
      loggedIn: false,
    };
  }
  return {
    episodes: formatStat(stats.episodesWatched),
    current: formatStat(stats.current),
    completed: formatStat(stats.completed),
    loggedIn: true,
  };
}

/**
 * Vitrin kartının başlığında gösterilen çıplak host.
 * "https://www.site.com/x" → "site.com". Geçersiz URL'lerde boş string —
 * kart bunu saklar.
 */
export function displayHost(url) {
  if (typeof url !== "string") return "";
  try {
    return new URL(url).host.replace(/^www\./, "");
  } catch {
    return "";
  }
}

const HEX = /^#([0-9a-f]{3}|[0-9a-f]{6})$/i;

function hexToRgb(hex) {
  const m = HEX.exec(hex || "");
  if (!m) return null;
  let h = m[1];
  if (h.length === 3) h = h.split("").map((c) => c + c).join("");
  const n = Number.parseInt(h, 16);
  return { r: (n >> 16) & 255, g: (n >> 8) & 255, b: n & 255 };
}

function rgbString({ r, g, b }) {
  return `${r} ${g} ${b}`;
}

function shade({ r, g, b }, factor) {
  const c = (v) => Math.max(0, Math.min(255, Math.round(v * factor)));
  return { r: c(r), g: c(g), b: c(b) };
}

/**
 * Site renginden kart banner'ı: renk → koyuya doğru 135° gradyan + sağ üstten
 * hafif beyaz ışıma. Geçersiz renk güvenli varsayılana düşer.
 * Dönen değer yalnızca doğrulanmış hex'lerden türetilir → CSS'e güvenle
 * atanabilir (kullanıcı stringi asla doğrudan geçmez).
 */
export function bannerStyle(color) {
  const rgb = hexToRgb(color) || hexToRgb("#6366f1");
  const deep = shade(rgb, 0.42);
  const mid = shade(rgb, 0.75);
  return (
    "linear-gradient(135deg," +
    `rgb(${rgbString(rgb)}) 0%,` +
    `rgb(${rgbString(mid)}) 46%,` +
    `rgb(${rgbString(deep)}) 100%)`
  );
}

/**
 * Hover halkası/gölgesi için CSS değişkenine yazılacak `r g b` üçlüsü —
 * `color-mix` içinde kullanılır.
 */
export function bannerRgb(color) {
  const rgb = hexToRgb(color) || hexToRgb("#6366f1");
  return rgbString(rgb);
}
