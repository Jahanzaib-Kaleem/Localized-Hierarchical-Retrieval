import type { ImportColumnSchema, ImportDatasetSchema, LogicalType } from '../../api/types'

export type CsvPreview = {
  headers: string[]
  rows: string[][]
  schema: ImportDatasetSchema
}

function parseRows(text: string, maxRows: number): string[][] {
  const rows: string[][] = []
  let row: string[] = []
  let value = ''
  let quoted = false

  for (let i = 0; i < text.length && rows.length < maxRows; i += 1) {
    const char = text[i]
    if (quoted) {
      if (char === '"') {
        if (text[i + 1] === '"') { value += '"'; i += 1 }
        else quoted = false
      } else value += char
      continue
    }
    if (char === '"') quoted = true
    else if (char === ',') { row.push(value); value = '' }
    else if (char === '\n') {
      row.push(value.replace(/\r$/, ''))
      value = ''
      if (row.some((cell) => cell.length > 0)) rows.push(row)
      row = []
    } else value += char
  }

  if (!quoted && rows.length < maxRows && (value.length > 0 || row.length > 0)) {
    row.push(value.replace(/\r$/, ''))
    if (row.some((cell) => cell.length > 0)) rows.push(row)
  }
  return rows
}

function inferType(values: string[]): LogicalType {
  const present = values.map((value) => value.trim()).filter(Boolean)
  if (present.length === 0) return 'text'
  if (present.every((value) => /^\d+$/.test(value))) return 'unsigned'
  if (present.every((value) => /^-?\d+$/.test(value))) return 'signed'
  if (present.every((value) => /^(true|false)$/i.test(value))) return 'boolean'
  return 'text'
}

function inferColumn(name: string, values: string[]): ImportColumnSchema {
  const nullable = values.some((value) => value.trim() === '')
  return {
    name,
    logical_type: inferType(values),
    nullable,
    normalization: 'trim',
    null_values: nullable ? [''] : [],
  }
}

export async function previewCsv(file: File): Promise<CsvPreview> {
  const text = await file.slice(0, 1024 * 1024).text()
  const parsed = parseRows(text, 101)
  if (parsed.length < 2) throw new Error('CSV needs a header row and at least one data row.')
  const headers = parsed[0].map((header, index) => header.trim() || `column_${index + 1}`)
  if (new Set(headers).size !== headers.length) throw new Error('CSV contains duplicate column names.')
  const rows = parsed.slice(1).filter((row) => row.length === headers.length)
  if (rows.length === 0) throw new Error('Could not find complete data rows matching the CSV header.')
  const columns = headers.map((name, index) => inferColumn(name, rows.map((row) => row[index] ?? '')))
  return { headers, rows: rows.slice(0, 8), schema: { format: 'LHR-SCHEMA/1', columns } }
}
