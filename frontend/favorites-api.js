// @ts-check

import { invoke } from "./tauri-api.js";

/** @typedef {import("./provider-command-names.js").RemoteProvider} RemoteProvider */
/** @typedef {"local" | RemoteProvider} FavoriteProvider */
/** @typedef {{name: string, path: string, provider: FavoriteProvider, profile_id?: string}} Favorite */
/** @typedef {{provider: FavoriteProvider, path: string, profileId: string | null}} FavoriteEndpoint */
/** @typedef {{name: string, path: string}} LocalFavoriteRequest */
/** @typedef {LocalFavoriteRequest & {provider: RemoteProvider, profileId: string}} RemoteFavoriteRequest */
/** @typedef {FavoriteEndpoint & {index: number}} MoveFavoriteRequest */
/** @typedef {{
 *   favorites: Favorite[],
 *   added: number,
 *   skipped_existing: number,
 *   skipped_missing: number,
 *   skipped_hidden: number,
 *   skipped_unreadable: number,
 * }} FinderImportSummary */

/** @returns {Promise<Favorite[]>} */
export function listFavorites() {
  return invoke("list_favorites");
}

/** @param {LocalFavoriteRequest} request @returns {Promise<Favorite[]>} */
export function addLocalFavorite(request) {
  return invoke("add_favorite", request);
}

/** @param {RemoteFavoriteRequest} request @returns {Promise<Favorite[]>} */
export function addRemoteFavorite(request) {
  return invoke("add_remote_favorite", request);
}

/** @param {FavoriteEndpoint} endpoint @returns {Promise<Favorite[]>} */
export function removeFavorite(endpoint) {
  return invoke("remove_favorite", endpoint);
}

/** @param {MoveFavoriteRequest} request @returns {Promise<Favorite[]>} */
export function moveFavorite(request) {
  return invoke("move_favorite", request);
}

/** @returns {Promise<FinderImportSummary>} */
export function importFinderFavorites() {
  return invoke("import_finder_favorites");
}
