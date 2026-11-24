// One `LogStream` per instance, shared by every view showing it. The server
// keeps one log cursor per instance per WebSocket, so two streams of the
// same instance would fight over it (and each unsubscribe would end both).
import { LogStream, type LogConnection, type StreamOptions } from './stream.svelte'

interface Entry {
  stream: LogStream
  users: number
}

export class StreamPool {
  private readonly entries = new Map<string, Entry>()
  private readonly connection: LogConnection
  private readonly options: StreamOptions

  constructor(connection: LogConnection, options: StreamOptions = {}) {
    this.connection = connection
    this.options = options
  }

  /** The stream of an instance, started; pair with `release`. */
  acquire(project: string, instance: string): LogStream {
    const key = `${project}/${instance}`
    let entry = this.entries.get(key)
    if (entry === undefined) {
      entry = { stream: new LogStream(this.connection, project, instance, this.options), users: 0 }
      this.entries.set(key, entry)
      entry.stream.start()
    }
    entry.users += 1
    return entry.stream
  }

  /**
   * Gives a stream back. The last user stops it, after the current task:
   * a view replaced by another of the same instance (a page change, a pane
   * moved) keeps the rows and the subscription.
   */
  release(stream: LogStream): void {
    const key = `${stream.project}/${stream.instance}`
    const entry = this.entries.get(key)
    if (entry?.stream !== stream || entry.users === 0) return
    entry.users -= 1
    if (entry.users > 0) return
    queueMicrotask(() => {
      if (this.entries.get(key) !== entry || entry.users > 0) return
      this.entries.delete(key)
      entry.stream.stop()
    })
  }

  /** Streams currently held (for tests). */
  get size(): number {
    return this.entries.size
  }
}
