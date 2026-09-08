import type { PhotoUpload } from '../api'
import { createClientId } from './clientId'

export const PHOTO_ACCEPT = 'image/jpeg,image/png,image/gif,image/webp'
export const MAX_PHOTOS = 10
export const MAX_PHOTO_BYTES = 2 * 1024 * 1024
export const MAX_TOTAL_PHOTO_BYTES = 12 * 1024 * 1024

const SUPPORTED_PHOTO_TYPES = new Set(PHOTO_ACCEPT.split(','))

export type SelectedPhoto = PhotoUpload & {
  id: string
  preview_url: string
  size_bytes: number
}

export class PhotoSelectionError extends Error {
  constructor(public readonly code: 'conversation.invalid_photo' | 'conversation.photo_too_large' | 'conversation.too_many_photos') {
    super(code)
  }
}

export async function preparePhotoFiles(files: File[], selected: SelectedPhoto[] = []): Promise<SelectedPhoto[]> {
  if (selected.length + files.length > MAX_PHOTOS) throw new PhotoSelectionError('conversation.too_many_photos')
  let total = selected.reduce((sum, photo) => sum + photo.size_bytes, 0)
  for (const file of files) {
    if (!SUPPORTED_PHOTO_TYPES.has(file.type)) throw new PhotoSelectionError('conversation.invalid_photo')
    total += file.size
    if (file.size > MAX_PHOTO_BYTES || total > MAX_TOTAL_PHOTO_BYTES) throw new PhotoSelectionError('conversation.photo_too_large')
  }

  return Promise.all(files.map(async (file) => {
    const data_base64 = bytesToBase64(new Uint8Array(await file.arrayBuffer()))
    return {
      id: createClientId(),
      filename: file.name || 'photo',
      mime_type: file.type,
      data_base64,
      preview_url: `data:${file.type};base64,${data_base64}`,
      size_bytes: file.size,
    }
  }))
}

export function photoUploadPayload(photos: SelectedPhoto[]): PhotoUpload[] {
  return photos.map(({ filename, mime_type, data_base64 }) => ({ filename, mime_type, data_base64 }))
}


function bytesToBase64(bytes: Uint8Array) {
  const chunkSize = 0x8000
  let binary = ''
  for (let offset = 0; offset < bytes.length; offset += chunkSize) {
    binary += String.fromCharCode(...bytes.subarray(offset, offset + chunkSize))
  }
  return btoa(binary)
}
