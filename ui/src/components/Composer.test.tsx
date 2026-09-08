import { createElement } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it, vi } from 'vitest'
import { Composer } from './Composer'
import type { SelectedPhoto } from '../shared/photoUploads'

const photo: SelectedPhoto = {
  id: 'photo-1',
  filename: 'garden.jpg',
  mime_type: 'image/jpeg',
  data_base64: '/9j/',
  preview_url: 'data:image/jpeg;base64,/9j/',
  size_bytes: 3,
}

function render(supportsPhotoUploads: boolean, photos: SelectedPhoto[] = []) {
  return renderToStaticMarkup(createElement(Composer, {
    value: '',
    disabled: false,
    busy: false,
    commands: [],
    photos,
    supportsPhotoUploads,
    onChange: vi.fn(),
    onAddPhotos: vi.fn(),
    onRemovePhoto: vi.fn(),
    onSend: vi.fn(),
  }))
}

describe('Composer photo controls', () => {
  it('hides uploads when the server does not advertise support', () => {
    const html = render(false)
    expect(html).not.toContain('type="file"')
    expect(html).not.toContain('Add photos')
  })

  it('shows a multiple image picker when uploads are supported', () => {
    const html = render(true)
    expect(html).toContain('type="file"')
    expect(html).toContain('accept="image/jpeg,image/png,image/gif,image/webp"')
    expect(html).toContain('multiple=""')
    expect(html).toContain('aria-label="Add photos"')
  })

  it('renders removable previews and enables a photo-only send', () => {
    const html = render(true, [photo])
    expect(html).toContain('aria-label="Selected photos"')
    expect(html).toContain('src="data:image/jpeg;base64,/9j/"')
    expect(html).toContain('aria-label="Remove photo: garden.jpg"')
    expect(html).toMatch(/<button class="send-button" aria-label="Send">/)
  })
})
