import { fireEvent, render, screen, within } from '@testing-library/svelte'
import { describe, expect, it, vi } from 'vitest'
import { instanceView, live, project } from '../../test-fixtures'
import type { InstanceSettings, InstanceSettingsInfo, InstanceView } from '../api'
import SettingsDrawer from './SettingsDrawer.svelte'
import { SettingsDrawerStore } from './settings.svelte'

const SAVED: InstanceSettings = { reload_mode: 'auto', autofix_mode: 'auto', extra_args: [], env: {} }

function setup(view: InstanceView = instanceView('main')) {
  const info = (settings: InstanceSettings): InstanceSettingsInfo => ({
    settings,
    project_env: ['PGHOST'],
    managed_options: ['--http-port', '-p'],
  })
  const api = {
    getInstanceSettings: vi.fn(() => Promise.resolve(info(SAVED))),
    putInstanceSettings: vi.fn((_p: string, _i: string, body: InstanceSettings) => Promise.resolve(info(body))),
  }
  const shop = project('shop', [view], { has_venv: true })
  const projects = { find: vi.fn((name: string) => (name === 'shop' ? shop : undefined)), load: vi.fn(() => Promise.resolve()) }
  const actions = { run: vi.fn(() => Promise.resolve()), busy: vi.fn(() => undefined) }
  const drawer = new SettingsDrawerStore()
  render(SettingsDrawer, { drawer, api, projects, actions })
  return { drawer, api, projects, actions }
}

async function open(drawer: SettingsDrawerStore): Promise<HTMLElement> {
  drawer.show('shop', 'main')
  const dialog = await screen.findByRole('dialog', { name: 'Instance settings' })
  await within(dialog).findByText('Smart')
  return dialog
}

describe('SettingsDrawer', () => {
  it('shows the saved settings of the instance it opens on', async () => {
    const { drawer, api } = setup()
    expect(screen.queryByRole('dialog')).toBeNull()
    const dialog = await open(drawer)
    expect(api.getInstanceSettings).toHaveBeenCalledWith('shop', 'main')
    expect(dialog).toHaveAccessibleDescription('shop / main')
    expect(within(dialog).getByRole('group', { name: 'Smart Reload' })).toBeInTheDocument()
    expect(within(dialog).getByRole('radio', { name: /^Smart/ })).toBeChecked()
    expect(within(dialog).getByRole('radio', { name: /^Automatic/ })).toBeChecked()
    expect(within(dialog).getByRole('textbox', { name: 'Extra odoo-bin arguments' })).toHaveValue('')
    expect(within(dialog).getByText(/project's PGHOST/)).toBeInTheDocument()
    expect(within(dialog).getByRole('button', { name: 'Save' })).toBeDisabled()
    await vi.waitFor(() => expect(dialog).toHaveFocus())
  })

  it('saves changes; a stopped instance uses them at the next start', async () => {
    const { drawer, api, projects } = setup()
    const dialog = await open(drawer)
    await fireEvent.click(within(dialog).getByRole('radio', { name: /^Off Files are not watched/ }))
    await fireEvent.input(within(dialog).getByRole('textbox', { name: 'Extra odoo-bin arguments' }), {
      target: { value: '--dev=xml\n--log-level=debug' },
    })
    await fireEvent.click(within(dialog).getByRole('button', { name: 'Add variable' }))
    await fireEvent.input(within(dialog).getByRole('textbox', { name: 'Name of variable 1' }), { target: { value: 'STAGE' } })
    await fireEvent.input(within(dialog).getByRole('textbox', { name: 'Value of STAGE' }), { target: { value: 'dev' } })
    await fireEvent.click(within(dialog).getByRole('button', { name: 'Save' }))
    await within(dialog).findByText('Saved. The new settings are used at the next start.')
    expect(api.putInstanceSettings).toHaveBeenCalledWith('shop', 'main', {
      reload_mode: 'off',
      autofix_mode: 'auto',
      extra_args: ['--dev=xml', '--log-level=debug'],
      env: { STAGE: 'dev' },
    })
    expect(projects.load).toHaveBeenCalled()
  })

  it('offers a restart when the instance runs here', async () => {
    const { drawer, actions } = setup(instanceView('main', { live: live('shop', 'main', { state: 'running', pid: 3 }) }))
    const dialog = await open(drawer)
    await fireEvent.click(within(dialog).getByRole('radio', { name: /^Ask first/ }))
    await fireEvent.click(within(dialog).getByRole('button', { name: 'Save' }))
    await within(dialog).findByText('Saved. Restart main to use the new settings.')
    await fireEvent.click(within(dialog).getByRole('button', { name: 'Restart now' }))
    expect(actions.run).toHaveBeenCalledWith('restart', 'shop', 'main')
  })

  it('shows problems instead of saving', async () => {
    const { drawer, api } = setup()
    const dialog = await open(drawer)
    const args = within(dialog).getByRole('textbox', { name: 'Extra odoo-bin arguments' })
    await fireEvent.input(args, { target: { value: '--http-port=9000' } })
    expect(args).toHaveAttribute('aria-invalid', 'true')
    expect(args).toHaveAccessibleDescription(expect.stringContaining('is set by OdHive'))
    await fireEvent.input(args, { target: { value: '' } })
    await fireEvent.click(within(dialog).getByRole('button', { name: 'Add variable' }))
    const name = within(dialog).getByRole('textbox', { name: 'Name of variable 1' })
    await fireEvent.input(name, { target: { value: 'BAD-NAME' } })
    expect(name).toHaveAccessibleDescription('Letters, digits and _ only, not starting with a digit')
    await fireEvent.click(within(dialog).getByRole('button', { name: 'Save' }))
    expect(api.putInstanceSettings).not.toHaveBeenCalled()
    await fireEvent.click(within(dialog).getByRole('button', { name: 'Remove BAD-NAME' }))
    expect(within(dialog).queryByRole('textbox', { name: /Name of variable/ })).toBeNull()
  })

  it('shows the server refusal', async () => {
    const { drawer, api } = setup()
    api.putInstanceSettings.mockRejectedValueOnce(new Error('project shop is being set up'))
    const dialog = await open(drawer)
    await fireEvent.click(within(dialog).getByRole('radio', { name: /^Restart only/ }))
    await fireEvent.click(within(dialog).getByRole('button', { name: 'Save' }))
    expect(await within(dialog).findByRole('alert')).toHaveTextContent('project shop is being set up')
  })

  it('closes with Esc and gives focus back; asks before dropping changes', async () => {
    const opener = document.createElement('button')
    document.body.append(opener)
    opener.focus()
    const { drawer } = setup()
    let dialog = await open(drawer)
    await fireEvent.keyDown(window, { key: 'Escape' })
    expect(screen.queryByRole('dialog')).toBeNull()
    expect(opener).toHaveFocus()

    opener.focus()
    dialog = await open(drawer)
    await fireEvent.click(within(dialog).getByRole('radio', { name: /^Off Files/ }))
    await fireEvent.keyDown(window, { key: 'Escape' })
    expect(within(dialog).getByText('Discard unsaved changes?')).toBeInTheDocument()
    await vi.waitFor(() => expect(within(dialog).getByRole('button', { name: 'Keep editing' })).toHaveFocus())
    // Esc again keeps editing.
    await fireEvent.keyDown(window, { key: 'Escape' })
    expect(within(dialog).queryByText('Discard unsaved changes?')).toBeNull()
    expect(within(dialog).getByRole('radio', { name: /^Off Files/ })).toBeChecked()
    await fireEvent.click(within(dialog).getByRole('button', { name: 'Cancel' }))
    await fireEvent.click(within(dialog).getByRole('button', { name: 'Discard' }))
    expect(screen.queryByRole('dialog')).toBeNull()
    expect(opener).toHaveFocus()
    opener.remove()
  })

  it('keeps Tab inside the drawer', async () => {
    const { drawer } = setup()
    const dialog = await open(drawer)
    const close = within(dialog).getByRole('button', { name: 'Close' })
    // Save is disabled while nothing changed, so Cancel is the last stop.
    expect(within(dialog).getByRole('button', { name: 'Save' })).toBeDisabled()
    const cancel = within(dialog).getByRole('button', { name: 'Cancel' })
    cancel.focus()
    await fireEvent.keyDown(window, { key: 'Tab' })
    expect(close).toHaveFocus()
    await fireEvent.keyDown(window, { key: 'Tab', shiftKey: true })
    expect(cancel).toHaveFocus()
  })
})
