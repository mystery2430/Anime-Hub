/**
 * Pure focus-wrap rule for modal dialogs.
 *
 * `count` is the number of focusable items inside the open dialog, `index` the
 * position of the focused one (-1 when focus is outside the dialog), and
 * `shift` whether Shift was held with Tab.
 *
 * Returns the index that should receive focus, or `null` when the browser's
 * default Tab movement is already correct and must not be intercepted.
 */
export function wrapFocusIndex(count, index, shift) {
  if (count <= 0) return null;
  if (index < 0 || index >= count) return shift ? count - 1 : 0;
  if (shift && index === 0) return count - 1;
  if (!shift && index === count - 1) return 0;
  return null;
}
