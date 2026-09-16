export type ApiEnvelope<T> = {
  request_id: number
  result: T
}

export type HealthResponse = {
  status: 'ok'
}

export type ReadyResponse = {
  status: 'ready' | 'not_ready'
  error?: string
}

export type ColumnStats = {
  id: number
  name: string
  cardinality: number
  nullable: boolean
  logical_type: string
  dictionary_bytes: number
}

export type IndexInfo = {
  file: string
  columns: number[]
  column_names: string[]
  kind: string
  entries: number
  keyspace: number
  bytes: number
  exact_rows: boolean
}

export type DatasetStats = {
  rows: number
  columns: number
  pages: number
  canonical_bytes: number
  routing_bytes: number
  total_bytes: number
  column_stats: ColumnStats[]
  indexes: IndexInfo[]
}

export type ApiFailure = {
  error: string
  request_id?: number
}
