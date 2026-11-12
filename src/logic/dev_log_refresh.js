// Gated, latest-request-wins access to the developer diagnostics buffer.
// Async reads must not repopulate the panel after it is closed or the saved
// developer_options gate is turned off.
export function createDeveloperLogReader({
  isEnabled,
  isVisible,
  read,
  reset,
  onSnapshot,
  onError,
}) {
  let generation = 0;
  // Reads started during a reset wait until the reset transaction has settled,
  // so they cannot display a pre-reset snapshot after a newer reset request.
  let resetBarrier = Promise.resolve();

  const isAllowed = () => Boolean(isEnabled()) && Boolean(isVisible());
  const isCurrent = (request) => request === generation && isAllowed();

  async function refresh() {
    const request = ++generation;
    if (!isAllowed()) return false;
    const barrier = resetBarrier;
    try {
      await barrier;
      if (!isCurrent(request)) return false;
      const snapshot = await read();
      if (!isCurrent(request)) return false;
      onSnapshot(snapshot, "refresh");
      return true;
    } catch (error) {
      if (isCurrent(request)) onError(error, "refresh");
      return false;
    }
  }

  function resetAndRefresh() {
    const request = ++generation;
    if (!isAllowed()) return Promise.resolve(false);

    const operation = resetBarrier.then(async () => {
      if (!isAllowed()) return false;
      try {
        await reset();
        if (!isCurrent(request)) return false;
        const snapshot = await read();
        if (!isCurrent(request)) return false;
        onSnapshot(snapshot, "reset");
        return true;
      } catch (error) {
        if (isCurrent(request)) onError(error, "reset");
        return false;
      }
    });
    resetBarrier = operation.then(
      () => undefined,
      () => undefined,
    );
    return operation;
  }

  function invalidate() {
    generation += 1;
  }

  return { refresh, resetAndRefresh, invalidate };
}
