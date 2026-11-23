// The WebSocket to the service: one per tab, reconnects with backoff.
// Stores re-subscribe in `onopen` handlers, which run on every (re)connect.
import { WS_PATH, type ClientMessage, type ServerMessage } from './api'

export type LiveStatus = 'idle' | 'connecting' | 'open' | 'reconnecting' | 'stopped'

/** The part of `WebSocket` the connection uses (tests pass a fake). */
export interface SocketLike {
  readonly readyState: number
  onopen: ((event: Event) => void) | null
  onmessage: ((event: MessageEvent) => void) | null
  onclose: ((event: CloseEvent) => void) | null
  onerror: ((event: Event) => void) | null
  send(data: string): void
  close(code?: number, reason?: string): void
}

const OPEN = 1

/** Waits before reconnect attempt 1, 2, …; the last value repeats. */
export const BACKOFF_MS = [500, 1000, 2000, 5000, 10_000]

export interface LiveOptions {
  url?: string
  createSocket?: (url: string) => SocketLike
  backoff?: number[]
}

/** `ws://` or `wss://` URL of the WebSocket on the page's own host. */
export function socketUrl(location: Pick<Location, 'protocol' | 'host'> = window.location): string {
  const scheme = location.protocol === 'https:' ? 'wss' : 'ws'
  return `${scheme}://${location.host}${WS_PATH}`
}

function parse(data: unknown): ServerMessage | null {
  if (typeof data !== 'string') return null
  try {
    const message: unknown = JSON.parse(data)
    if (typeof message === 'object' && message !== null && typeof (message as { type?: unknown }).type === 'string') {
      return message as ServerMessage
    }
  } catch {
    // Not JSON: ignore.
  }
  return null
}

export class LiveConnection {
  status = $state<LiveStatus>('idle')
  /** Failed attempts since the last open connection. */
  failures = $state(0)
  /** OdHive version from the server's `hello`. */
  version = $state<string | null>(null)

  private socket: SocketLike | null = null
  private timer: ReturnType<typeof setTimeout> | null = null
  private readonly url: () => string
  private readonly createSocket: (url: string) => SocketLike
  private readonly backoff: number[]
  // Listeners, not UI state: plain sets on purpose.
  // eslint-disable-next-line svelte/prefer-svelte-reactivity
  private readonly messageListeners = new Set<(message: ServerMessage) => void>()
  // eslint-disable-next-line svelte/prefer-svelte-reactivity
  private readonly openListeners = new Set<() => void>()

  constructor(options: LiveOptions = {}) {
    this.url = () => options.url ?? socketUrl()
    this.createSocket = options.createSocket ?? ((url) => new WebSocket(url))
    this.backoff = options.backoff ?? BACKOFF_MS
  }

  /** Every message from the server. Returns the unsubscribe function. */
  onmessage(listener: (message: ServerMessage) => void): () => void {
    this.messageListeners.add(listener)
    return () => this.messageListeners.delete(listener)
  }

  /** Runs on every open connection, first and after reconnects. */
  onopen(listener: () => void): () => void {
    this.openListeners.add(listener)
    return () => this.openListeners.delete(listener)
  }

  start(): void {
    if (this.status !== 'idle' && this.status !== 'stopped') return
    this.connect()
  }

  stop(): void {
    this.status = 'stopped'
    this.clearTimer()
    const socket = this.socket
    this.socket = null
    if (socket) {
      this.detach(socket)
      socket.close(1000, 'closed by the page')
    }
  }

  /** Sends now if connected; returns false otherwise (resend in `onopen`). */
  send(message: ClientMessage): boolean {
    if (!this.socket || this.socket.readyState !== OPEN || this.status !== 'open') return false
    this.socket.send(JSON.stringify(message))
    return true
  }

  /** Tries again at once, e.g. when the tab becomes visible. */
  retryNow(): void {
    if (this.status !== 'reconnecting') return
    this.clearTimer()
    this.connect()
  }

  private connect(): void {
    this.status = this.failures === 0 ? 'connecting' : 'reconnecting'
    let socket: SocketLike
    try {
      socket = this.createSocket(this.url())
    } catch {
      this.scheduleReconnect()
      return
    }
    this.socket = socket
    socket.onopen = () => {
      if (this.socket !== socket) return
      this.status = 'open'
      this.failures = 0
      for (const listener of [...this.openListeners]) listener()
    }
    socket.onmessage = (event) => {
      if (this.socket !== socket) return
      const message = parse(event.data)
      if (!message) return
      if (message.type === 'hello') this.version = message.version
      for (const listener of [...this.messageListeners]) listener(message)
    }
    socket.onclose = () => {
      if (this.socket !== socket) return
      this.socket = null
      this.detach(socket)
      this.scheduleReconnect()
    }
    // A failed connection also gets `close`, which reconnects.
    socket.onerror = () => {}
  }

  private scheduleReconnect(): void {
    if (this.status === 'stopped') return
    this.failures += 1
    this.status = 'reconnecting'
    const delay = this.backoff[Math.min(this.failures, this.backoff.length) - 1] ?? 1000
    this.clearTimer()
    this.timer = setTimeout(() => {
      this.timer = null
      this.connect()
    }, delay)
  }

  private clearTimer(): void {
    if (this.timer !== null) clearTimeout(this.timer)
    this.timer = null
  }

  private detach(socket: SocketLike): void {
    socket.onopen = null
    socket.onmessage = null
    socket.onclose = null
    socket.onerror = null
  }
}
