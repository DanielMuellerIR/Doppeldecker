// @ts-check

/** @typedef {import("./provider-command-names.js").RemoteProvider} RemoteProvider */
/** @typedef {{provider: "local", path: string} | {provider: RemoteProvider, path: string, profileId: string, profileRevision?: number | null}} PaneEndpointLocation */
/** @typedef {{provider: "local", root: string} | {provider: RemoteProvider, root: string, profileId: string, profileRevision?: number | null}} BidirectionalEndpointRequest */

/**
 * @param {PaneEndpointLocation} pane
 * @returns {BidirectionalEndpointRequest}
 */
export function paneEndpoint(pane) {
  return pane.provider === "local"
    ? { provider: "local", root: pane.path }
    : { provider: pane.provider, profileId: pane.profileId,
      profileRevision: pane.profileRevision, root: pane.path };
}
