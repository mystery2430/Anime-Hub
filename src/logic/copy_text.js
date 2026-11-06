// Minimal browser clipboard fallback for secure-context WebViews that do not
// expose navigator.clipboard. Always remove the temporary node, even when
// selection or execCommand throws.
export function copyTextFallback(text, documentRef = globalThis.document) {
  let textarea;
  try {
    textarea = documentRef.createElement("textarea");
    textarea.value = text;
    textarea.setAttribute("readonly", "");
    textarea.style.position = "fixed";
    textarea.style.opacity = "0";
    documentRef.body.append(textarea);
    textarea.select();
    return documentRef.execCommand("copy") === true;
  } finally {
    try {
      textarea?.remove();
    } catch {
      try {
        if (textarea?.parentNode) textarea.parentNode.removeChild(textarea);
      } catch {
        // Best-effort cleanup must not replace the clipboard outcome.
      }
    }
  }
}
