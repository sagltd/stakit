import typescript from '@rollup/plugin-typescript'

// JS bundles only; type declarations are emitted by `tsc -p tsconfig.build.json`
// into dist/types. preserveModules keeps the dist tree mirroring src/.
export default {
  input: 'src/index.ts',
  output: [
    {
      dir: 'dist/esm',
      format: 'es',
      preserveModules: true,
      preserveModulesRoot: 'src',
      entryFileNames: '[name].js',
    },
    {
      dir: 'dist/cjs',
      format: 'cjs',
      preserveModules: true,
      preserveModulesRoot: 'src',
      entryFileNames: '[name].cjs',
    },
  ],
  plugins: [
    typescript({
      tsconfig: './tsconfig.json',
      declaration: false,
      outDir: undefined,
    }),
  ],
}
