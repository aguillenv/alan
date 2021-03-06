import { asyncopcodes, } from 'alan-js-runtime'

import {
  LP,
  LPNode,
  LPError,
  NamedAnd,
  NulLP,
} from './lp'

import amm from './amm'

const callToJsText = (call: LPNode) => {
  const args = call.has('calllist') ?
    call.get('calllist').getAll().map(r => r.get('variable').t).join(', ') : ""
  const opcode = call.get('variable').t
  return asyncopcodes.includes(opcode) ? `await r.${opcode}(${args})` : `r.${opcode}(${args})`
}

const functionbodyToJsText = (fnbody: LPNode, indent: string) => {
  let outText = ""
  for (const statement of fnbody.get('statements').getAll()) {
    outText += indent + "  " // For legibility of the output
    if (statement.has('declarations')) {
      if (statement.get('declarations').has('constdeclaration')) {
        const dec = statement.get('declarations').get('constdeclaration')
        outText += `const ${dec.get('decname').t} = ${assignableToJsText(dec.get('assignables'), dec.get('fulltypename'), indent)}\n`
      } else if (statement.get('declarations').has('letdeclaration')) {
        const dec = statement.get('declarations').get('letdeclaration')
        outText += `let ${dec.get('decname').t} = ${assignableToJsText(dec.get('assignables'), dec.get('fulltypename'), indent)}\n`
      }
    } else if (statement.has('assignments')) {
      const assign = statement.get('assignments')
      outText += `${assign.get('decname').t} = ${assignableToJsText(assign.get('assignables'), assign.get('fulltypename'), indent)}\n`
    } else if (statement.has('calls')) {
      outText += `${callToJsText(statement.get('calls'))}\n`
    } else if (statement.has('emits')) {
      const emit = statement.get('emits')
      const name = emit.get('variable').t
      const arg = emit.has('value') ? emit.get('value').get('variable').t : 'undefined'
      outText += `r.emit('${name}', ${arg})\n`
    } else if (statement.has('exits')) {
      outText += `${statement.get('exits').t.trim()}\n`
    }
  }
  return outText
}

const assignableToJsText = (assignable: LPNode, fulltypename: LPNode, indent: string) => {
  let outText = ""
  if (assignable.has('functions')) {
    const args = assignable.get('functions').get('args')
    const argnames = []
    for (const arg of args.get(0).getAll()) {
      argnames.push(arg.get('arg').get('variable').t)
    }
    if (args.get(1)) {
      argnames.push(args.get(1).get('arg').get('variable').t)
    }
    outText += `async (${argnames.join(', ')}) => {\n`
    outText += functionbodyToJsText(assignable.get('functions').get('functionbody'), indent + "  ")
    outText += indent + '  }' // End this closure
  } else if (assignable.has('calls')) {
    outText += callToJsText(assignable.get('calls'))
  } else if (assignable.has('variable')) {
    outText += assignable.get('variable').t
  } else if (assignable.has('value')) {
    outText += assignable.get('value').t
    try {
      const t = assignable.get('value').t
      if (!/"/.test(t) && t !== 'true' && t !== 'false' && !/\./.test(t)) {
        parseInt(assignable.get('value').t)
        if (fulltypename.t.trim() === 'int64') {
          outText += 'n'
        }
      }
    } catch (e) { }
  }
  return outText
}

const ammToJsText = (amm: LPNode) => {
  let outFile = "const r = require('alan-js-runtime')\n"
  // Where we're going we don't need types, so skipping that entire section
  // First convert all of the global constants to javascript
  for (const globalConst of amm.get('globalMem').getAll()) {
    const rec = globalConst.get()
    if (!(rec instanceof NamedAnd)) continue
    outFile +=
      `const ${rec.get('decname').t} = ${assignableToJsText(rec.get('assignables'), rec.get('fulltypename'), '')}\n`
  }
  // We can also skip the event declarations because they are lazily bound by EventEmitter
  // Now we convert the handlers to Javascript. This is the vast majority of the work
  let hasConn = false
  let hasTcpConn = false
  for (const handler of amm.get('handlers').getAll()) {
    const rec = handler.get()
    if (!(rec instanceof NamedAnd)) continue
    let arg = rec.get('functions').get('args').get(0).get(0).get('arg')
    if (arg instanceof NulLP) {
      arg = rec.get('functions').get('args').get(1).get('arg')
    }
    const eventVarName = !(arg instanceof NulLP) ?
      arg.get('variable').t : ""
    if (rec.get('variable').t === '__conn') hasConn = true
    if (rec.get('variable').t === '__ctrl') throw new Error('AVM Control Port unavailable in JS')
    if (rec.get('variable').t === 'tcpConn') hasTcpConn = true
    outFile += `r.on('${rec.get('variable').t}', async (${eventVarName}) => {\n`
    outFile += functionbodyToJsText(rec.get('functions').get('functionbody'), '')
    outFile += '})\n' // End this handler
  }
  if (hasConn) outFile += "r.on('_start', () => r.httplsn())\n" // Make sure a web server starts up
  if (hasTcpConn) outFile += "r.on('_start', () => r.tcplsn())\n" // Make sure a server starts up
  outFile += "r.emit('_start', undefined)\n" // Let's get it started in here
  return outFile
}

export const fromFile = (filename: string) => {
  const lp = new LP(filename)
  const ast = amm.apply(lp)
  if (ast instanceof LPError) {
    throw new Error(ast.msg)
  }
  return ammToJsText(ast)
}
export const fromString = (str: string) => {
  const lp = LP.fromText(str)
  const ast = amm.apply(lp)
  if (ast instanceof LPError) {
    throw new Error(ast.msg)
  }
  return ammToJsText(ast)
}
