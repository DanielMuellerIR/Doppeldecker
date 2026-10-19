/** Beendet nur den eigenen Mock-Server und den eigenen Playwright-Browser. */
export function createBrowserCleanup(server) {
  let browserPromise;
  let closing;
  let serverClosed = false;
  let startError;
  const serverExit = new Promise(resolve => {
    server.once("close", () => { serverClosed = true; resolve(); });
  });
  server.once("error", error => { startError = error; });

  async function waitForServerExit() {
    let timer;
    try {
      await Promise.race([
        serverExit,
        new Promise(resolve => { timer = setTimeout(resolve, 2000); }),
      ]);
    } finally {
      clearTimeout(timer);
    }
  }

  function close() {
    closing ||= (async () => {
      const results = await Promise.allSettled([
        (async () => {
          if (!serverClosed) server.kill("SIGTERM");
          await waitForServerExit();
          if (!serverClosed) {
            server.kill("SIGKILL");
            await waitForServerExit();
            if (!serverClosed) throw new Error("Eigener Mock-Server wurde nicht beendet.");
          }
        })(),
        (async () => { await (await browserPromise)?.close(); })(),
      ]);
      const failure = results.find(result => result.status === "rejected");
      if (failure) throw failure.reason;
    })();
    return closing;
  }

  const signals = { SIGINT: 130, SIGTERM: 143, SIGHUP: 129 };
  const handlers = new Map(Object.entries(signals).map(([name, code]) => {
    const handler = () => {
      // Der Server muss schon vor einem möglicherweise noch laufenden
      // Browserstart sein Signal erhalten. Playwrights Exit-Cleanup räumt bei
      // Überschreiten der Frist auch seine getrennte Browser-Prozessgruppe auf.
      server.kill("SIGTERM");
      const deadline = setTimeout(() => process.exit(code), 5000);
      void close().catch(error => console.error(error)).finally(() => {
        clearTimeout(deadline);
        process.exit(code);
      });
    };
    process.on(name, handler);
    return [name, handler];
  }));

  return {
    checkServer() {
      if (startError) throw startError;
      if (server.exitCode !== null || server.signalCode !== null) {
        throw new Error(`Mock-Server endete mit ${server.exitCode ?? server.signalCode}.`);
      }
    },
    launch(browserType, options) {
      browserPromise = browserType.launch({
        ...options, handleSIGINT: false, handleSIGTERM: false, handleSIGHUP: false,
      });
      return browserPromise;
    },
    async close() {
      try {
        await close();
      } finally {
        for (const [name, handler] of handlers) process.off(name, handler);
      }
    },
  };
}
