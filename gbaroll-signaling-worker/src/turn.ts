// Cloudflare TURN: the deployment holds a TURN key (id + API token) as
// worker secrets and mints short-lived credentials to hand out in each
// `Hello`. https://developers.cloudflare.com/realtime/turn/

import { type IceServer, iceServer } from "./protocol.ts";

export interface TurnEnv {
  /** TURN key id, from the dashboard's Realtime → TURN server page. */
  CLOUDFLARE_TURN_TOKEN_ID?: string;
  /** The key's API token. Both are secrets (`wrangler secret put`). */
  CLOUDFLARE_TURN_API_TOKEN?: string;
  /** Optional var: minted-credential lifetime in seconds. TURN
   * allocations die when the credential expires, so this must outlast a
   * whole session, not just the handshake. */
  TURN_CRED_TTL?: string;
}

export const DEFAULT_CRED_TTL_SECONDS = 24 * 60 * 60;

/** Handed out when no TURN key is configured or minting fails:
 * STUN-only, like the native server's default list. */
export const FALLBACK_ICE_SERVERS: IceServer[] = [
  iceServer(["stun:stun.cloudflare.com:3478", "stun:stun.l.google.com:19302"]),
];

interface ApiIceServer {
  urls: string | string[];
  username?: string;
  credential?: string;
}

/** Mint ICE servers (STUN + TURN with short-lived credentials) from the
 * Cloudflare TURN API. Returns null when no TURN key is configured;
 * throws when minting fails. */
export async function generateIceServers(env: TurnEnv): Promise<IceServer[] | null> {
  const { CLOUDFLARE_TURN_TOKEN_ID: keyId, CLOUDFLARE_TURN_API_TOKEN: token } = env;
  if (!keyId || !token) return null;
  const ttl = env.TURN_CRED_TTL ? Number(env.TURN_CRED_TTL) : DEFAULT_CRED_TTL_SECONDS;
  const res = await fetch(
    `https://rtc.live.cloudflare.com/v1/turn/keys/${keyId}/credentials/generate-ice-servers`,
    {
      method: "POST",
      headers: {
        authorization: `Bearer ${token}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({ ttl }),
    },
  );
  if (!res.ok) {
    throw new Error(`TURN credential mint failed: ${res.status} ${await res.text()}`);
  }
  const body = (await res.json()) as { iceServers?: ApiIceServer[] };
  const servers: IceServer[] = [];
  for (const s of body.iceServers ?? []) {
    const urls = (Array.isArray(s.urls) ? s.urls : [s.urls])
      // Browsers block the port-53 variants; they'd only stall ICE.
      .filter((u) => !/:53(\?|$)/.test(u));
    if (urls.length === 0) continue;
    servers.push(iceServer(urls, s.username, s.credential));
  }
  if (servers.length === 0) {
    throw new Error("TURN credential mint returned no usable ice servers");
  }
  return servers;
}
