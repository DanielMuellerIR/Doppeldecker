import type { BidirectionalEndpointRequest } from "./pane-endpoint.js";

export type BidirectionalPairIdWire = [
  number, number, number, number, number, number, number, number,
  number, number, number, number, number, number, number, number,
];

export type BidirectionalDigestWire = [
  number, number, number, number, number, number, number, number,
  number, number, number, number, number, number, number, number,
  number, number, number, number, number, number, number, number,
  number, number, number, number, number, number, number, number,
];

type BinaryRelativePathWire = number[][];
type EndpointSideWire = "left" | "right";

type ConflictDecisionWire =
  | { decision: "keep_left" | "keep_right" | "skip" }
  | { decision: "keep_both"; primary: EndpointSideWire; conflict_path: BinaryRelativePathWire };

export interface ConflictResolutionWire {
  path: BinaryRelativePathWire;
  decision: ConflictDecisionWire;
}

export interface RegisteredBidirectionalPairWire {
  pairId: BidirectionalPairIdWire;
  pairName: string;
  left: BidirectionalEndpointRequest;
  right: BidirectionalEndpointRequest;
  visibleLeftRole: EndpointSideWire;
}

export interface PrepareBidirectionalRequestWire {
  pairId: BidirectionalPairIdWire;
  pairName: string;
  left: BidirectionalEndpointRequest;
  right: BidirectionalEndpointRequest;
  resolutions: ConflictResolutionWire[];
  expectedUnresolvedDigest?: BidirectionalDigestWire | null;
}

interface ConflictEntryWire {
  endpoint: string;
  exists: boolean;
  kind: string | null;
  size: number | null;
  mtime: number | null;
}

export interface BidirectionalConflictWire {
  path: BinaryRelativePathWire;
  relative_name: string;
  left: ConflictEntryWire;
  right: ConflictEntryWire;
  choices: Array<{
    decision: ConflictDecisionWire;
    explanation: string;
    generated_name: string | null;
    blocker: string | null;
  }>;
  skip_blocker: string | null;
}

type DirectionWire = "left_to_right" | "right_to_left";
type ConflictCopyPathWire = {
  source_path: BinaryRelativePathWire;
  target_path: BinaryRelativePathWire;
  expected_source: BidirectionalDigestWire;
};
type BidirectionalActionWire =
  | ({ type: "conflict_copy"; source_side: EndpointSideWire; target_side: EndpointSideWire;
      subtree: ConflictCopyPathWire[] } & ConflictCopyPathWire)
  | { type: "copy"; path: BinaryRelativePathWire; direction: DirectionWire;
      expected_source: BidirectionalDigestWire; expected_target: BidirectionalDigestWire | null }
  | { type: "type_swap"; path: BinaryRelativePathWire; direction: DirectionWire;
      staging_path: BinaryRelativePathWire; expected_source: BidirectionalDigestWire;
      expected_target: BidirectionalDigestWire; subtree: ConflictCopyPathWire[] }
  | { type: "delete"; path: BinaryRelativePathWire; direction: DirectionWire; expected_target: BidirectionalDigestWire }
  | { type: "converged"; path: BinaryRelativePathWire; entry_digest: BidirectionalDigestWire }
  | { type: "tombstone"; path: BinaryRelativePathWire; previous_entry_digest: BidirectionalDigestWire }
  | ({
      path: BinaryRelativePathWire;
      baseline: BidirectionalDigestWire | null;
      left: BidirectionalDigestWire | null;
      right: BidirectionalDigestWire | null;
    } & (
      | { type: "conflict"; kind: "initial_difference" | "both_changed" | "changed_against_deletion" |
          "type_changed" | "recreated_after_deletion" }
      | { type: "skipped_conflict" }
    ));

type RootIdentityWire = { provider: "Local" | "Sftp" | "Ftps" | "WebDav" | "Smb"; identity: unknown };

interface BidirectionalPlanWire {
  pair_id: BidirectionalPairIdWire;
  baseline_generation: number | null;
  baseline_digest: BidirectionalDigestWire | null;
  left_root_identity: RootIdentityWire;
  right_root_identity: RootIdentityWire;
  left_snapshot_digest: BidirectionalDigestWire;
  right_snapshot_digest: BidirectionalDigestWire;
  actions: BidirectionalActionWire[];
  conflicts: number;
  executable: boolean;
  plan_digest: BidirectionalDigestWire;
}

export interface LocalBidirectionalPreviewWire {
  plan: BidirectionalPlanWire;
  conflict_details: BidirectionalConflictWire[];
  scope_warnings: string[];
  stage_blocker: string | null;
  confirmation_phrase: string;
  expires_at_unix_ms: number;
  backup_confirmation_required: boolean;
}

export interface ForwardBidirectionalPairStatusWire {
  execution_class: string;
  run_id: number[];
  plan: BidirectionalPlanWire;
  projections: unknown[];
  baseline_matches_previous: boolean;
  baseline_completion_available: boolean;
  previous_runs: number;
}

export interface ForwardBidirectionalPreviewWire {
  plan: BidirectionalPlanWire;
  conflict_details: BidirectionalConflictWire[];
  scope_warnings: string[];
  stage_blocker: string | null;
  unresolved_digest: BidirectionalDigestWire;
  digest: BidirectionalDigestWire;
  execution_class: string;
  expires_at_unix_ms: number;
  limitations: string[];
  recovery: {
    previous_run_id: number[];
    changed_left: BinaryRelativePathWire[];
    changed_right: BinaryRelativePathWire[];
    running_projections: number[];
    warning: string;
  } | null;
  effects: ForwardBidirectionalEffectWire[];
}

// Projektion der vom Forward-Dialog gelesenen Content-Sync-Wirkungen.
interface ForwardBidirectionalEffectWire {
  target_identity: RootIdentityWire;
  target_root: string;
  plan: {
    copy_count: number;
    update_count: number;
    delete_count: number;
    bytes_to_transfer: number;
    metadata_report: { issues: Array<{ path: string; class: string; reason: string }> };
  };
  irreversible_replacements: number;
  irreversible_removals: number;
  quarantined_replacements: number;
  quarantined_removals: number;
  quarantine_run: string | null;
  blocked_removals: number;
  removal_confirmation: string | null;
  limitations: string[];
}

export type BidirectionalRecoveryKindWire = "resume" | "rollback";

export interface BidirectionalResumePreviewWire extends LocalBidirectionalPreviewWire {
  recovery_preparation_id: string;
  recovery_kind: "resume";
}

export interface BidirectionalRollbackPreviewWire {
  pair_id: BidirectionalPairIdWire;
  committed_projections: number;
  confirmation_phrase: string;
  expires_at_unix_ms: number;
  recovery_preparation_id: string;
  recovery_kind: "rollback";
}

export interface BidirectionalRecoveryDiscardWire {
  absent: boolean;
}

export interface ForwardBidirectionalConfirmationWire {
  effectsConfirmed: boolean;
  preparationDigest: BidirectionalDigestWire;
  removalPhrases: string[];
}
