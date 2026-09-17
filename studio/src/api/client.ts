import type {
  ApiEnvelope, ApiFailure, CsvImportReport, DatasetStats, GenerationInfo, HealthResponse, ImportDatasetSchema,
  IndexChangeReport, MutationOperation, MutationReport, QueryRequest, QueryResponse, ReadyResponse, VacuumReport,
  WorkloadReport,
} from './types'

const TOKEN_KEY = 'lhr.studio.api-token'

export class ApiError extends Error {
  readonly status: number
  readonly requestId?: number
  constructor(message: string, status: number, requestId?: number) {
    super(message)
    this.name = 'ApiError'
    this.status = status
    this.requestId = requestId
  }
}

export function getApiToken(): string { return sessionStorage.getItem(TOKEN_KEY) ?? '' }
export function setApiToken(token: string): void {
  const value = token.trim()
  if (value) sessionStorage.setItem(TOKEN_KEY, value)
  else sessionStorage.removeItem(TOKEN_KEY)
  window.dispatchEvent(new Event('lhr-token-change'))
}

async function request<T>(path: string, init: RequestInit = {}, authenticated = true): Promise<T> {
  const headers = new Headers(init.headers)
  const token = getApiToken()
  if (init.body && !(init.body instanceof FormData) && !headers.has('content-type')) headers.set('content-type', 'application/json')
  if (authenticated && token) headers.set('authorization', `Bearer ${token}`)
  const response = await fetch(path, { ...init, headers, credentials: 'same-origin' })
  const contentType = response.headers.get('content-type') ?? ''
  const payload = contentType.includes('application/json') ? await response.json() : await response.text()
  if (!response.ok) {
    const failure = typeof payload === 'object' && payload !== null ? (payload as ApiFailure) : undefined
    throw new ApiError(failure?.error ?? `Request failed with HTTP ${response.status}`, response.status, failure?.request_id)
  }
  return payload as T
}

const json = (value: unknown) => JSON.stringify(value)

export const api = {
  health: (signal?: AbortSignal) => request<HealthResponse>('/healthz', { signal }, false),
  ready: (signal?: AbortSignal) => request<ReadyResponse>('/readyz', { signal }, false),
  stats: async (signal?: AbortSignal) => (await request<ApiEnvelope<DatasetStats>>('/v1/stats', { signal })).result,
  workload: async (signal?: AbortSignal) => (await request<ApiEnvelope<WorkloadReport>>('/v1/workload', { signal })).result,
  generations: async (signal?: AbortSignal) => (await request<ApiEnvelope<GenerationInfo[]>>('/v1/generations', { signal })).result,
  metrics: (signal?: AbortSignal) => request<string>('/metrics', { signal }),
  query: async (body: QueryRequest, signal?: AbortSignal) => (await request<ApiEnvelope<QueryResponse>>('/v1/query', { method: 'POST', body: json(body), signal })).result,
  importCsv: async (file: File, schema: ImportDatasetSchema) => {
    const form = new FormData()
    form.append('schema', JSON.stringify(schema))
    form.append('file', file, file.name)
    return (await request<ApiEnvelope<CsvImportReport>>('/v1/admin/import/csv', { method: 'POST', body: form })).result
  },
  mutate: async (mutations: MutationOperation[]) => (await request<ApiEnvelope<MutationReport>>('/v1/mutate', { method: 'POST', body: json({ mutations }) })).result,
  indexChange: async (action: 'add' | 'drop' | 'rebuild', columns: string[]) => (await request<ApiEnvelope<IndexChangeReport>>(`/v1/admin/index/${action}`, { method: 'POST', body: json({ columns }) })).result,
  compact: async () => (await request<ApiEnvelope<unknown>>('/v1/admin/compact', { method: 'POST', body: json({}) })).result,
  vacuum: async (retain: number) => (await request<ApiEnvelope<VacuumReport>>('/v1/admin/vacuum', { method: 'POST', body: json({ retain, protect: [] }) })).result,
  recover: async () => (await request<ApiEnvelope<unknown>>('/v1/admin/recover', { method: 'POST', body: json({}) })).result,
}
