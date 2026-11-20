import { render, screen } from '@testing-library/svelte'
import { describe, expect, it } from 'vitest'
import App from './App.svelte'

describe('App', () => {
  it('renders the product name and tagline', () => {
    render(App)
    expect(screen.getByRole('heading', { name: 'OdHive' })).toBeInTheDocument()
    expect(screen.getByText('Your Odoo servers, one hive.')).toBeInTheDocument()
  })
})
