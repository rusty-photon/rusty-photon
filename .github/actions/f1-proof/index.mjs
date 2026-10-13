// F1 proof: an entry file no test imports, importing a name that does not exist.
import { getInputs } from '../lib/helpers.mjs';

console.log(getInputs('x'));
