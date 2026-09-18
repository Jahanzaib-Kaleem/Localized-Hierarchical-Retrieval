import type {
  ApiEnvelope, ApiFailure, BucketCombineReport, BucketInfo, BucketTransferReport, CsvImportReport,
  DatasetStats, GenerationInfo, HealthResponse, ImportDatasetSchema, IndexChangeReport, MutationOperation,
  MutationReport, QueryRequest, QueryResponse, ReadyResponse, VacuumReport, WorkloadReport,
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

const bucketQuery = (path: string, bucket = 'default') => `${path}?bucket=${encodeURIComponent(bucket)}`

export const api = {
  health: (signal?: AbortSignal) => request<HealthResponse>('/healthz', { signal }, false),
  ready: (signal?: AbortSignal) => request<ReadyResponse>('/readyz', { signal }, false),
  buckets: async (signal?: AbortSignal) => (await request<ApiEnvelope<BucketInfo[]>>('/v1/buckets', { signal })).result,
  stats: async (signal?: AbortSignal, bucket = 'default') => (await request<ApiEnvelope<DatasetStats>>(bucketQuery('/v1/stats', bucket), { signal })).result,
  workload: async (signal?: AbortSignal, bucket = 'default') => (await request<ApiEnvelope<WorkloadReport>>(bucketQuery('/v1/workload', bucket), { signal })).result,
  generations: async (signal?: AbortSignal, bucket = 'default') => (await request<ApiEnvelope<GenerationInfo[]>>(bucketQuery('/v1/generations', bucket), { signal })).result,
  metrics: (signal?: AbortSignal) => request<string>('/metrics', { signal }),
  query: async (body: QueryRequest, signal?: AbortSignal) => (await request<ApiEnvelope<QueryResponse>>('/v1/query', { method: 'POST', body: json(body), signal })).result,
  importCsv: async (file: File, schema: ImportDatasetSchema, bucket = 'default') => {
    const form = new FormData()
    form.append('bucket', bucket)
    form.append('schema', JSON.stringify(schema))
    form.append('file', file, file.name)
    return (await request<ApiEnvelope<CsvImportReport>>('/v1/admin/import/csv', { method: 'POST', body: form })).result
  },
  mutate: async (mutations: MutationOperation[], bucket = 'default') => (await request<ApiEnvelope<MutationReport>>('/v1/mutate', { method: 'POST', body: json({ bucket, mutations }) })).result,
  indexChange: async (action: 'add' | 'drop' | 'rebuild', columns: string[], bucket = 'default') => (await request<ApiEnvelope<IndexChangeReport>>(`/v1/admin/index/${action}`, { method: 'POST', body: json({ bucket, columns }) })).result,
  compact: async (bucket = 'default') => (await request<ApiEnvelope<unknown>>('/v1/admin/compact', { method: 'POST', body: json({ bucket }) })).result,
  vacuum: async (retain: number, bucket = 'default') => (await request<ApiEnvelope<VacuumReport>>('/v1/admin/vacuum', { method: 'POST', body: json({ bucket, retain, protect: [] }) })).result,
  recover: async (bucket = 'default') => (await request<ApiEnvelope<unknown>>(bucketQuery('/v1/admin/recover', bucket), { method: 'POST' })).result,
  createBucket: async (id: string, name: string) => (await request<ApiEnvelope<BucketInfo>>('/v1/admin/buckets/create', { method: 'POST', body: json({ id, name }) })).result,
  renameBucket: async (id: string, name: string) => (await request<ApiEnvelope<BucketInfo>>('/v1/admin/buckets/rename', { method: 'POST', body: json({ id, name }) })).result,
  deleteBucket: async (id: string) => (await request<ApiEnvelope<{ deleted: string }>>('/v1/admin/buckets/delete', { method: 'POST', body: json({ id, confirm: id }) })).result,
  combineBuckets: async (sources: string[], targetId: string, targetName: string) => (await request<ApiEnvelope<BucketCombineReport>>('/v1/admin/buckets/combine', { method: 'POST', body: json({ sources, target_id: targetId, target_name: targetName }) })).result,
  transferRows: async (source: string, destination: string, rowIds: number[], moveRows = false) => (await request<ApiEnvelope<BucketTransferReport>>('/v1/buckets/transfer', { method: 'POST', body: json({ source, destination, row_ids: rowIds, move_rows: moveRows }) })).result,
}
