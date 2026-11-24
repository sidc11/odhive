// Types of the OdHive REST API. `schema.d.ts` and `openapi.json` are
// generated from the server (`pnpm api:gen`); never edit them by hand.
import type { components, operations, paths } from './schema'

export type { components, operations, paths }

type Schemas = components['schemas']

/** Every API route is below this prefix. */
export const API_PREFIX = '/api/v1'

/** Header (value `1`) every request other than GET/HEAD must carry. */
export const MUTATION_HEADER = 'X-OdHive'

export type ApiError = Schemas['ErrorBody']
export type Health = Schemas['Health']
export type SourceEntry = Schemas['SourceEntry']
export type NewSource = Schemas['NewSource']
export type SchemaInfo = Schemas['SchemaInfo']
export type OdooOption = Schemas['OdooOption']
export type Project = Schemas['Project']
export type CreateProject = Schemas['CreateProject']
export type NewInstance = Schemas['NewInstance']
export type PythonList = Schemas['PythonList']
export type PythonInfo = Schemas['PythonInfo']
/** Environment check: PostgreSQL, Python, wkhtmltopdf, git, uv. */
export type EnvCheckReport = Schemas['EnvCheckReport']
export type EnvCheck = Schemas['EnvCheck']
export type EnvStatus = Schemas['EnvStatus']
export type EnvFix = Schemas['EnvFix']
export type EnvCheckId = Schemas['EnvCheckId']
export type InstanceView = Schemas['InstanceView']
export type InstanceInfo = Schemas['InstanceInfo']
export type InstanceUsage = Schemas['InstanceUsage']
export type PortChange = Schemas['PortChange']
export type GitHead = Schemas['GitHead']
export type ExternalRun = Schemas['ExternalRun']
export type Status = Schemas['Status']
export type StopResult = Schemas['StopResult']
export type ExitInfo = Schemas['ExitInfo']
export type Candidate = Schemas['Candidate']
export type LogPage = Schemas['LogPage']
export type LogRecord = Schemas['Record']

/** WebSocket of live logs and instance events (below the API prefix). */
export const WS_PATH = `${API_PREFIX}/ws`

/** What the web UI sends over the WebSocket. */
export type ClientMessage = Schemas['ClientMessage']
/** What the server sends over the WebSocket. */
export type ServerMessage = Schemas['ServerMessage']
export type ManagerEvent = Schemas['ManagerEvent']
/** What to install for a missing system library, as text. */
export type Hint = Schemas['Hint']

/** Background work (source clones, project setups) and its progress. */
export type Job = Schemas['Job']
export type JobEvent = Schemas['JobEvent']
export type JobStep = Schemas['JobStep']
export type CloneSource = Schemas['CloneSource']
export type SetupRequest = Schemas['SetupRequest']

/** One-time code that signs a browser in at `path` (`/login?t=…`). */
export type LoginCode = Schemas['LoginCode']
