import { afterEach, describe, expect, it, vi } from 'vitest'
import type { InstanceSettings, InstanceSettingsInfo } from '../api'
import { ApiRequestError } from '../api/client'
import { SettingsDrawerStore, SettingsForm } from './settings.svelte'

const SAVED: InstanceSettings = { reload_mode: 'auto', autofix_mode: 'auto', extra_args: ['--dev=xml'], env: { STAGE: 'dev' } }

function info(settings: InstanceSettings = SAVED): InstanceSettingsInfo {
  return { settings, project_env: ['PGHOST', 'STAGE'], managed_options: ['--http-port', '-p'] }
}

function fakeApi() {
  return {
    getInstanceSettings: vi.fn(() => Promise.resolve(info())),
    putInstanceSettings: vi.fn((_p: string, _i: string, body: InstanceSettings) => Promise.resolve(info(body))),
  }
}

async function loaded(api = fakeApi()) {
  const form = new SettingsForm(api, 'shop', 'main')
  await form.load()
  return { form, api }
}

describe('SettingsForm', () => {
  it('loads the saved settings into the fields', async () => {
    const { form, api } = await loaded()
    expect(api.getInstanceSettings).toHaveBeenCalledWith('shop', 'main')
    expect(form.status).toBe('ready')
    expect(form.argsText).toBe('--dev=xml')
    expect(form.envRows.map((r) => [r.key, r.value])).toEqual([['STAGE', 'dev']])
    expect(form.overridden).toEqual(['STAGE'])
    expect(form.dirty).toBe(false)
    expect(form.canSave).toBe(false)
  })

  it('is dirty only while it differs from what is saved', async () => {
    const { form } = await loaded()
    form.reloadMode = 'off'
    expect(form.dirty).toBe(true)
    form.reloadMode = 'auto'
    form.argsText = '--dev=xml\n\n'
    expect(form.dirty).toBe(false)
    form.addEnvRow()
    expect(form.dirty).toBe(false)
  })

  it('saves the parsed form and reports it saved until the next change', async () => {
    const { form, api } = await loaded()
    form.autofixMode = 'ask'
    form.argsText = '--log-level=debug\n-u\nsale'
    const row = form.addEnvRow()
    row.key = 'TOKEN'
    row.value = 's3cret'
    expect(await form.save()).toBe(true)
    expect(api.putInstanceSettings).toHaveBeenCalledWith('shop', 'main', {
      reload_mode: 'auto',
      autofix_mode: 'ask',
      extra_args: ['--log-level=debug', '-u', 'sale'],
      env: { STAGE: 'dev', TOKEN: 's3cret' },
    })
    expect(form.savedNow).toBe(true)
    expect(form.dirty).toBe(false)
    form.reloadMode = 'off'
    expect(form.savedNow).toBe(false)
  })

  it('does not send settings the server would refuse', async () => {
    const { form, api } = await loaded()
    form.argsText = '-p9000'
    expect(form.argNotes[0]?.blocking).toBe(true)
    expect(await form.save()).toBe(false)
    form.argsText = '--dev=xml'
    const bad = form.addEnvRow()
    bad.key = '9LIVES'
    expect(await form.save()).toBe(false)
    expect(form.tried).toBe(true)
    expect(api.putInstanceSettings).not.toHaveBeenCalled()
    // A space in an option is only a hint.
    form.removeEnvRow(bad.id)
    form.argsText = '--dev xml'
    expect(form.canSave).toBe(true)
  })

  it('keeps the edits and shows the server message when saving fails', async () => {
    const api = fakeApi()
    api.putInstanceSettings.mockRejectedValueOnce(new ApiRequestError(409, 'setup_running', 'project shop is being set up'))
    const { form } = await loaded(api)
    form.reloadMode = 'restart'
    expect(await form.save()).toBe(false)
    expect(form.saveError).toBe('project shop is being set up')
    expect(form.reloadMode).toBe('restart')
    expect(form.dirty).toBe(true)
    expect(await form.save()).toBe(true)
    expect(form.saveError).toBeNull()
  })

  it('reports a failed load', async () => {
    const api = fakeApi()
    api.getInstanceSettings.mockRejectedValueOnce(new ApiRequestError(404, 'unknown_instance', 'no instance x'))
    const { form } = await loaded(api)
    expect(form.status).toBe('error')
    expect(form.loadError).toBe('no instance x')
    expect(form.canSave).toBe(false)
  })
})

describe('SettingsDrawerStore', () => {
  afterEach(() => {
    document.body.innerHTML = ''
  })

  it('opens on an instance and gives focus back on hide', () => {
    const button = document.createElement('button')
    document.body.append(button)
    button.focus()
    const drawer = new SettingsDrawerStore()
    drawer.show('shop', 'main')
    expect(drawer.target).toEqual({ project: 'shop', instance: 'main' })
    // Switching instances keeps the original focus target.
    document.body.focus()
    drawer.show('shop', 'test')
    expect(drawer.target).toEqual({ project: 'shop', instance: 'test' })
    button.blur()
    drawer.hide()
    expect(drawer.target).toBeNull()
    expect(button).toHaveFocus()
  })
})
