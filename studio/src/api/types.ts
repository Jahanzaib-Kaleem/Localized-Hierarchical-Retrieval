export type ApiEnvelope<T> = { request_id: number; result: T }
export type ApiFailure = { error: string; request_id?: number }
export type HealthResponse = { status: 'ok' }
export type ReadyResponse = { status: 'ready' | 'not_ready'; error?: string }

export type BucketInfo = {
  id: string
  name: string
  is_default: boolean
  ready: boolean
  rows: number
  columns: number
  total_bytes: number
}

export type BucketCombineReport = {
  target: BucketInfo
  sources: string[]
  rows: number
}

export type BucketTransferReport = {
  source: string
  destination: string
  mode: 'copy' | 'move'
  rows_requested: number
  destination_report: MutationReport
  source_report: MutationReport | null
  warning: string | null
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

export type LogicalType = 'text' | 'unsigned' | 'signed' | 'boolean' | 'timestamp'
export type Normalization = 'none' | 'trim' | 'lowercase' | 'trim_lowercase'
export type ImportColumnSchema = {
  name: string
  logical_type: LogicalType
  nullable: boolean
  normalization: Normalization
  null_values: string[]
}
export type ImportDatasetSchema = { format: 'LHR-SCHEMA/1'; columns: ImportColumnSchema[] }
export type CsvImportReport = {
  generation: GenerationInfo
  rows: number
  cardinalities: number[]
  exact_hierarchies: number
}

export type NamedValue = { column: string; value: string | null }
export type QueryFilter =
  | { op: 'eq'; column: string; value: string | null }
  | { op: 'in'; column: string; values: Array<string | null> }
  | { op: 'range'; column: string; gte?: string | null; lte?: string | null }

export type QueryRequest = {
  bucket?: string
  filters: QueryFilter[]
  select?: string[]
  limit?: number
  after_row_id?: number | null
  max_rows_examined?: number
  timeout_ms?: number
}

export type QueryApiRow = { row_id: number; values: NamedValue[] }
export type QueryApiStats = {
  hits: number
  rows_examined: number
  pages_touched: number
  hierarchy_lookups: number
  elapsed_micros: number
  optimized_equality_route: boolean
}
export type QueryResponse = {
  rows: QueryApiRow[]
  returned: number
  next_cursor: number | null
  stats: QueryApiStats
}

export type GenerationInfo = { id: number; path: string; current: boolean; sealed: boolean }
export type VacuumReport = { kept: number[]; deleted: number[]; stale_paths_removed: number; bytes_reclaimed: number }

export type QueryShapeStats = {
  columns: string[]
  operators: string[]
  queries: number
  p50_micros: number
  p95_micros: number
  p99_micros: number
  avg_rows_examined: number
  max_rows_examined: number
}
export type IndexUseStats = { index: string; queries: number }
export type IndexRecommendation = {
  columns: string[]
  observed_queries: number
  avg_rows_examined: number
  score: number
  reason: string
}
export type WorkloadReport = {
  query_count: number
  p50_micros: number
  p95_micros: number
  p99_micros: number
  shapes: QueryShapeStats[]
  index_use: IndexUseStats[]
  recommendations: IndexRecommendation[]
}

export type MutationOperation =
  | { op: 'insert'; values: Record<string, string | null> }
  | { op: 'update'; row_id: number; values: Record<string, string | null> }
  | { op: 'delete'; row_id: number }

export type MutationReport = {
  generation: GenerationInfo
  rows_before: number
  rows_after: number
  inserted: number
  updated: number
  deleted: number
  max_row_id: number | null
}

export type IndexChangeReport = {
  generation: GenerationInfo
  action: string
  columns: string[]
  indexes: IndexInfo[]
}
