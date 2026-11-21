type Bytes16 = [number, number, number, number, number, number, number, number,
  number, number, number, number, number, number, number, number];
type Bytes32 = [...Bytes16, ...Bytes16];

interface KeychainSecretReferenceWire { service: string; account: string }
export interface CertificatePinWire { sha256: Bytes32 }
export interface HostKeyPinWire extends CertificatePinWire { algorithm: string }
export interface SftpPublicKeyIdentityWire extends HostKeyPinWire { comment: string }

export interface SmbServerDiscoveryWire {
  server_guid: Bytes16;
  dialect: number;
  encryption_capable: boolean;
  signing_required: boolean;
}

export interface CapturedConnectionSecretWire { secretId: string }

type CertificatePolicy = "web_pki" | "web_pki_and_pin" | "pinned_certificate" | "unverified_certificate";
type TransportSecurity = "plaintext" | "explicit_tls";
type FtpSecurityStatus = import("./scan-remote-dir-api.js").FtpSecurityStatus;

type SftpAuthenticationWire =
  | { type: "password"; secret: KeychainSecretReferenceWire }
  | { type: "private_key"; path: string; passphrase: KeychainSecretReferenceWire | null; public_key_sha256: Bytes32 }
  | { type: "ssh_agent"; public_key_sha256: Bytes32 };

interface AccountWire {
  username: string;
  requested_root: string;
}

interface HostPortWire { host: string; port: number }

interface SftpConnectionWire extends AccountWire, HostPortWire {
  alternate_hosts?: string[];
  host_key: HostKeyPinWire;
  authentication: SftpAuthenticationWire;
}

interface FtpsConnectionWire extends AccountWire, HostPortWire {
  alternate_hosts?: string[];
  transport_security: TransportSecurity;
  certificate?: CertificatePinWire;
  certificate_policy?: CertificatePolicy;
  password_reference: KeychainSecretReferenceWire;
}

interface WebDavConnectionWire extends AccountWire {
  origin: string;
  certificate: CertificatePinWire;
  certificate_policy: CertificatePolicy;
  password_reference: KeychainSecretReferenceWire;
}

interface SmbConnectionWire extends AccountWire, HostPortWire {
  server_guid: Bytes16;
  share: string;
  domain: string;
  password_reference: KeychainSecretReferenceWire;
}

interface ProfileWire<Connection> { id: string; name: string; connection: Connection }

export type SftpProfileWire = ProfileWire<SftpConnectionWire>;
export type FtpsProfileWire = ProfileWire<FtpsConnectionWire>;
export type WebDavProfileWire = ProfileWire<WebDavConnectionWire>;
export type SmbProfileWire = ProfileWire<SmbConnectionWire>;

type ProfileRequest<Profile extends ProfileWire<unknown>, Connection> =
  Omit<Profile, "connection"> & { connection: Connection };

type SftpAuthenticationRequest =
  Exclude<SftpAuthenticationWire, { type: "private_key" }>
  | (Omit<Extract<SftpAuthenticationWire, { type: "private_key" }>, "passphrase">
      & { passphrase?: KeychainSecretReferenceWire | null });

export type SftpProfileRequest = ProfileRequest<SftpProfileWire,
  Omit<SftpConnectionWire, "authentication"> & { authentication: SftpAuthenticationRequest }>;
export type FtpsProfileRequest = ProfileRequest<FtpsProfileWire,
  Omit<FtpsConnectionWire, "transport_security" | "certificate" | "certificate_policy">
  & { transport_security?: TransportSecurity; certificate?: CertificatePinWire | null; certificate_policy?: CertificatePolicy | null }>;

export type VersionedProfileWire<Profile> = Profile & { revision: number };

interface RemoteCapabilitiesWire {
  complete_listing: boolean;
  exclusive_directory_create: boolean;
  exclusive_create: boolean;
  atomic_noreplace_rename: boolean;
  best_effort_rename: boolean;
  best_effort_file_commit: boolean;
  best_effort_file_replace: boolean;
  atomic_replace_rename: boolean;
  durable_file_flush: boolean;
  verified_file_commit: boolean;
  durable_directory_flush: boolean;
  quarantine: boolean;
  entry_delete: boolean;
  permanent_delete: boolean;
  available_space: boolean;
}

interface ConnectionStatusWire<Identity> {
  profile_revision: number;
  identity: Identity;
  capabilities: RemoteCapabilitiesWire;
  quarantine_capabilities: { listing: boolean; restore: boolean; purge: boolean };
}

export type SftpConnectionStatusWire = ConnectionStatusWire<HostPortWire & {
  host_key: HostKeyPinWire;
  canonical_root: string;
  sftp_version: number;
  extensions_digest: Bytes32;
}>;

export type FtpsConnectionStatusWire = ConnectionStatusWire<HostPortWire & {
  transport_security: TransportSecurity;
  certificate?: CertificatePinWire;
  canonical_root: string;
  features_digest: Bytes32;
  welcome_digest: Bytes32;
}> & { security: FtpSecurityStatus };

export type WebDavConnectionStatusWire = ConnectionStatusWire<{
  origin: string;
  certificate: CertificatePinWire;
  account_digest: Bytes32;
  canonical_root: string;
  dav_header_digest: Bytes32;
  server_header_digest: Bytes32;
}>;

export type SmbConnectionStatusWire = ConnectionStatusWire<HostPortWire & {
  server_guid: Bytes16;
  share: string;
  account_digest: Bytes32;
  canonical_root: string;
  dialect: number;
  signing_active: boolean;
  encryption_active: boolean;
  signing_algorithm: string;
  encryption_cipher: string;
}>;
