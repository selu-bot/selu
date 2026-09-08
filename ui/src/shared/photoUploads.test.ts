import { describe, expect, it } from 'vitest'
import {
  MAX_PHOTO_BYTES,
  MAX_PHOTOS,
  photoUploadPayload,
  preparePhotoFiles,
  type SelectedPhoto,
} from './photoUploads'

function file(name: string, type: string, bytes: number[], size = bytes.length): File {
  const data = Uint8Array.from(bytes)
  return {
    name,
    type,
    size,
    arrayBuffer: async () => data.buffer,
  } as File
}

const selectedPhoto: SelectedPhoto = {
  id: 'selected',
  filename: 'selected.jpg',
  mime_type: 'image/jpeg',
  data_base64: '/9j/',
  preview_url: 'data:image/jpeg;base64,/9j/',
  size_bytes: 3,
}

describe('photo uploads', () => {
  it('prepares supported files for previews and the API payload', async () => {
    const [photo] = await preparePhotoFiles([file('tiny.png', 'image/png', [0, 255, 16])])

    expect(photo).toMatchObject({
      filename: 'tiny.png',
      mime_type: 'image/png',
      data_base64: 'AP8Q',
      preview_url: 'data:image/png;base64,AP8Q',
      size_bytes: 3,
    })
    expect(photo.id).toBeTruthy()
    expect(photoUploadPayload([photo])).toEqual([{
      filename: 'tiny.png',
      mime_type: 'image/png',
      data_base64: 'AP8Q',
    }])
  })

  it('rejects unsupported types before reading the file', async () => {
    const unsupported = file('photo.heic', 'image/heic', [], 20)
    await expect(preparePhotoFiles([unsupported])).rejects.toMatchObject({ code: 'conversation.invalid_photo' })
  })

  it('enforces the server count and decoded-size limits before encoding', async () => {
    await expect(preparePhotoFiles([file('large.jpg', 'image/jpeg', [], MAX_PHOTO_BYTES + 1)]))
      .rejects.toMatchObject({ code: 'conversation.photo_too_large' })
    await expect(preparePhotoFiles([file('one-more.jpg', 'image/jpeg', [1])], Array(MAX_PHOTOS).fill(selectedPhoto)))
      .rejects.toMatchObject({ code: 'conversation.too_many_photos' })
  })
})
