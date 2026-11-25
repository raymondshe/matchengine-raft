// Order-desk REST client, ported from `order_system/web/app.js`
// (`fetchJSON`/`postJSON` and the ticket bodies).
//
// Every POST carries `X-Demo-Action: submit`; the BFF rejects requests
// without it. Same-origin via the nginx proxy keeps the BFF's Origin check
// happy (Origin host == proxied Host).

import type {
  DeskBackends,
  DeskEventPage,
  DeskState,
  DeskWriteResult,
  PlaceBody,
} from "./types";

const TIMEOUT_MS = 5000;

export class DeskError extends Error {
  status: number;
  body: unknown;

  constructor(message: string, status: number, body: unknown) {
    super(message);
    this.status = status;
    this.body = body;
  }
}

async function fetchJSON<T>(url: string, options: RequestInit = {}): Promise<T> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), TIMEOUT_MS);
  try {
    const response = await fetch(url, {
      ...options,
      signal: controller.signal,
      headers: { "Content-Type": "application/json", ...(options.headers ?? {}) },
    });
    const body = (await response.json().catch(() => null)) as
      | (T & { error?: { message?: string } })
      | null;
    if (!response.ok) {
      const message = body?.error?.message ?? `${response.status} ${response.statusText}`;
      throw new DeskError(message, response.status, body);
    }
    return body as T;
  } finally {
    clearTimeout(timer);
  }
}

function postJSON<T>(url: string, body: unknown): Promise<T> {
  return fetchJSON<T>(url, {
    method: "POST",
    headers: { "X-Demo-Action": "submit" },
    body: JSON.stringify(body),
  });
}

export function getBackends(): Promise<DeskBackends> {
  return fetchJSON<DeskBackends>("/demo/api/backends");
}

export function getState(target: string, depth = 20, userId?: number): Promise<DeskState> {
  const user = userId === undefined ? "" : `&user_id=${encodeURIComponent(String(userId))}`;
  return fetchJSON<DeskState>(
    `/demo/api/state?depth=${depth}&target=${encodeURIComponent(target)}${user}`,
  );
}

export function getEvents(
  target: string,
  after: { log_index: number; ordinal: number } | null,
  limit: number,
): Promise<DeskEventPage> {
  const query = after
    ? `after_log_index=${after.log_index}&after_ordinal=${after.ordinal}&limit=${limit}&target=${encodeURIComponent(target)}`
    : `limit=${limit}&target=${encodeURIComponent(target)}`;
  return fetchJSON<DeskEventPage>(`/demo/api/events?${query}`);
}

export function place(body: PlaceBody): Promise<DeskWriteResult> {
  return postJSON<DeskWriteResult>("/demo/api/orders", body);
}

export function cancel(body: {
  user_id: number;
  target: string;
  side: string;
  price: string;
  sequence: number;
}): Promise<DeskWriteResult> {
  return postJSON<DeskWriteResult>("/demo/api/orders/cancel", body);
}

export function amend(body: {
  user_id: number;
  target: string;
  side: string;
  price: string;
  sequence: number;
  new_price?: string;
  new_volume?: string;
}): Promise<DeskWriteResult> {
  return postJSON<DeskWriteResult>("/demo/api/orders/amend", body);
}

export function freezePreview(body: PlaceBody): Promise<{
  economics?: {
    freeze?: { amount: string; asset: string; role: string; basis: string; fee_rate: string; reference_price: string };
    maker?: unknown;
    taker?: unknown;
    perpetual?: { leverage: number; position: string; notional: string; initial_margin: string; fee: { amount: string; asset: string }; buffer: string };
  };
  reason?: string;
}> {
  return postJSON("/demo/api/freeze", body);
}

export function newClientId(): string {
  if (window.crypto && typeof window.crypto.randomUUID === "function") {
    return `term-${window.crypto.randomUUID()}`;
  }
  return `term-${Date.now()}-${Math.random().toString(16).slice(2)}`;
}
