// @ts-check

import { invoke } from "./tauri-api.js";

/** @typedef {import("./pane-endpoint.js").BidirectionalEndpointRequest} BidirectionalEndpointRequest */
/** @typedef {{query: string, caseSensitive?: boolean, includeHidden?: boolean}} NameSearchOptions */
/** @typedef {{
 * side: "left" | "right", endpoint: BidirectionalEndpointRequest,
 * sessionGeneration?: number | null, options: NameSearchOptions, clientToken: number
 * }} NameSearchRequest */

/** @param {NameSearchRequest} request */
export const startNameSearch = request => invoke("start_name_search", { request });

/** @param {number} clientToken */
export const cancelNameSearch = clientToken => invoke("cancel_name_search", { clientToken });
