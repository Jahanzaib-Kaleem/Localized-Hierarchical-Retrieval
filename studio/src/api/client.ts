import type {
  ApiEnvelope, ApiFailure, BucketCombineReport, BucketInfo, BucketTransferReport, CsvImportResult,
  DatasetStats, GenerationInfo, HealthResponse, ImportDatasetSchema, ImportJobStatus, ImportMode,
  IndexChangeReport, MutationOperation, MutationReport, QueryRequest, QueryResponse, ReadyResponse,
  VacuumReport, WorkloadReport,
} from './types'

const TOKEN_KEY = 'lhr.studio.api-token'
const ACTIVE_IMPORT_KEY = 'lhr.studio.active-import'
const IMPORT_CHUNK_BYTES = 4 * 1024 * 1024

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


const delay = (ms: number) => new Promise<void>((resolve) => window.setTimeout(resolve, ms))

async function importJob(id: string): Promise<ImportJobStatus> {
  return (await request<ApiEnvelope<ImportJobStatus>>(`/v1/admin/imports/${encodeURIComponent(id)}`)).result
}

async function waitForImport(
  id: string,
  onProgress?: (job: ImportJobStatus) => void,
): Promise<CsvImportResult> {
  while (true) {
    const job = await importJob(id)
    onProgress?.(job)
    if (job.status === 'complete') {
      sessionStorage.removeItem(ACTIVE_IMPORT_KEY)
      if (!job.result) throw new ApiError('Import completed without a result.', 500)
      return job.result
    }
    if (job.status === 'failed') {
      sessionStorage.removeItem(ACTIVE_IMPORT_KEY)
      const safety = job.existing_dataset_preserved
        ? ' The previously published dataset was preserved.'
        : ' Check the current generation before retrying.'
      throw new ApiError((job.error ?? 'Import failed.') + safety, 422)
    }
    if (job.status === 'uploading') {
      throw new ApiError(
        `Upload is paused at ${job.bytes_received} of ${job.bytes_total} bytes. Re-select the same file to resume.`,
        409,
      )
    }
    await delay(800)
  }
}

async function fileFingerprint(file: File): Promise<string> {
  const bytes = await file.slice(0, 1024 * 1024).arrayBuffer()
  const digest = await crypto.subtle.digest('SHA-256', bytes)
  return Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, '0')).join('')
}

async function createOrResumeImport(
  file: File,
  schema: ImportDatasetSchema,
  bucket: string,
  mode: ImportMode,
  fingerprint: string,
): Promise<ImportJobStatus> {
  const active = sessionStorage.getItem(ACTIVE_IMPORT_KEY)
  if (active) {
    try {
      const job = await importJob(active)
      if (
        job.status === 'uploading'
        && job.bucket === bucket
        && job.mode === mode
        && job.file_name === file.name
        && job.file_fingerprint === fingerprint
        && job.bytes_total === file.size
      ) {
        return job
      }
      if (job.status !== 'complete' && job.status !== 'failed') {
        throw new ApiError(
          `Another import job (${job.id}) is already active for ${job.bucket}.`,
          409,
        )
      }
    } catch (error) {
      if (error instanceof ApiError && error.status === 409) throw error
      sessionStorage.removeItem(ACTIVE_IMPORT_KEY)
    }
  }

  const job = (await request<ApiEnvelope<ImportJobStatus>>('/v1/admin/imports', {
    method: 'POST',
    body: json({
      bucket,
      mode,
      schema,
      file_name: file.name,
      file_fingerprint: fingerprint,
      bytes_total: file.size,
    }),
  })).result
  sessionStorage.setItem(ACTIVE_IMPORT_KEY, job.id)
  return job
}

async function uploadImport(
  file: File,
  schema: ImportDatasetSchema,
  bucket: string,
  mode: ImportMode,
  onProgress?: (job: ImportJobStatus) => void,
): Promise<CsvImportResult> {
  const fingerprint = await fileFingerprint(file)
  let job = await createOrResumeImport(file, schema, bucket, mode, fingerprint)
  onProgress?.(job)

  while (job.bytes_received < file.size) {
    const offset = job.bytes_received
    const chunk = file.slice(offset, Math.min(file.size, offset + IMPORT_CHUNK_BYTES))
    try {
      job = (await request<ApiEnvelope<ImportJobStatus>>(
        `/v1/admin/imports/${encodeURIComponent(job.id)}/chunk?offset=${offset}`,
        {
          method: 'PUT',
          headers: { 'content-type': 'application/octet-stream' },
          body: chunk,
        },
      )).result
    } catch (error) {
      const current = await importJob(job.id)
      if (current.bytes_received <= offset) throw error
      job = current
    }
    onProgress?.(job)
  }

  job = (await request<ApiEnvelope<ImportJobStatus>>(
    `/v1/admin/imports/${encodeURIComponent(job.id)}/complete`,
    { method: 'POST' },
  )).result
  onProgress?.(job)
  return waitForImport(job.id, onProgress)
}

export const api = {
  health: (signal?: AbortSignal) => request<HealthResponse>('/healthz', { signal }, false),
  ready: (signal?: AbortSignal) => request<ReadyResponse>('/readyz', { signal }, false),
  buckets: async (signal?: AbortSignal) => (await request<ApiEnvelope<BucketInfo[]>>('/v1/buckets', { signal })).result,
  stats: async (signal?: AbortSignal, bucket = 'default') => (await request<ApiEnvelope<DatasetStats>>(bucketQuery('/v1/stats', bucket), { signal })).result,
  schema: async (signal?: AbortSignal, bucket = 'default') => (await request<ApiEnvelope<ImportDatasetSchema>>(bucketQuery('/v1/schema', bucket), { signal })).result,
  workload: async (signal?: AbortSignal, bucket = 'default') => (await request<ApiEnvelope<WorkloadReport>>(bucketQuery('/v1/workload', bucket), { signal })).result,
  generations: async (signal?: AbortSignal, bucket = 'default') => (await request<ApiEnvelope<GenerationInfo[]>>(bucketQuery('/v1/generations', bucket), { signal })).result,
  metrics: (signal?: AbortSignal) => request<string>('/metrics', { signal }),
  query: async (body: QueryRequest, signal?: AbortSignal) => (await request<ApiEnvelope<QueryResponse>>('/v1/query', { method: 'POST', body: json(body), signal })).result,
  importCsv: (
    file: File,
    schema: ImportDatasetSchema,
    bucket = 'default',
    mode: ImportMode = 'create',
    onProgress?: (job: ImportJobStatus) => void,
  ) => uploadImport(file, schema, bucket, mode, onProgress),
  importJob,
  waitForImport,
  activeImportId: () => sessionStorage.getItem(ACTIVE_IMPORT_KEY),
  clearActiveImport: () => sessionStorage.removeItem(ACTIVE_IMPORT_KEY),
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
